//! The remux: the mkvmerge command that rebuilds a file with some subtitle tracks replaced, each in
//! its original place with its original properties, and running it.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt::Write as _;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::ebml;
use crate::group;
use crate::mkv::{self, Encoding, Layout, Track};

/// A track taken from a file of its own: its mkvmerge ID in the original, the patched file, and
/// its own tags as mkvmerge's XML, if it has any (mkvmerge drops a left-out track's tags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub id: usize,
    pub file: PathBuf,
    pub tags: Option<PathBuf>,
}

/// mkvmerge's arguments for remuxing `original` (whose layout is `layout`) to `output`, with each
/// track of `replaced` taken from its file instead.
pub fn args(
    original: &Path,
    layout: &Layout,
    replaced: &[Replacement],
    output: &Path,
) -> Vec<OsString> {
    let mut out: Vec<OsString> = vec![
        "--quiet".into(),
        "-o".into(),
        output.into(),
        "--timestamp-scale".into(),
        SCALE.to_string().into(),
    ];
    if let Some(uid) = &layout.segment_uid {
        out.push("--segment-uid".into());
        out.push(hex(uid).into());
    }
    let ids: Vec<String> = replaced.iter().map(|r| r.id.to_string()).collect();
    out.push("--subtitle-tracks".into());
    out.push(format!("!{}", ids.join(",")).into());
    out.push(original.into());
    for r in replaced {
        let track = &layout.tracks[r.id];
        for (option, value) in track_options(track) {
            out.push(option.into());
            out.push(format!("0:{value}").into());
        }
        if let Some(tags) = &r.tags {
            out.push("--tags".into());
            let mut value = OsString::from("0:");
            value.push(tags);
            out.push(value);
        }
        out.push((&r.file).into());
    }
    let order: Vec<String> = layout
        .tracks
        .iter()
        .map(|t| match replaced.iter().position(|r| r.id == t.id) {
            Some(i) => format!("{}:0", i + 1),
            None => format!("0:{}", t.id),
        })
        .collect();
    out.push("--track-order".into());
    out.push(order.join(",").into());
    out
}

/// Bytes as lowercase hex, the way mkvmerge takes a segment UID.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The remux's TimestampScale, which [`args`] sets: mkvmerge's default (mkvmerge 102 remuxes a file
/// at 100 µs to 1 ms without it), so that the plan can count on it. The check reads the remux's own.
pub const SCALE: u64 = 1_000_000;

/// A replaced track's properties, as mkvmerge options for its file's track 0.
fn track_options(t: &Track) -> Vec<(&'static str, String)> {
    let yes = |b: bool| if b { "yes" } else { "no" }.to_string();
    let mut out = vec![(
        "--language",
        t.language_bcp47
            .clone()
            .unwrap_or_else(|| t.language.clone()),
    )];
    if let Some(name) = &t.name {
        out.push(("--track-name", name.clone()));
    }
    out.push(("--default-track-flag", yes(t.default)));
    out.push(("--forced-display-flag", yes(t.forced)));
    let f = &t.flags;
    for (option, flag) in [
        ("--track-enabled-flag", f.enabled),
        ("--hearing-impaired-flag", f.hearing_impaired),
        ("--visual-impaired-flag", f.visual_impaired),
        ("--text-descriptions-flag", f.text_descriptions),
        ("--original-flag", f.original),
        ("--commentary-flag", f.commentary),
    ] {
        if let Some(b) = flag {
            out.push((option, yes(b)));
        }
    }
    let zlib = t
        .encodings
        .iter()
        .any(|e| e.encoding == Encoding::Zlib && e.scope & mkv::SCOPE_FRAMES != 0);
    out.push(("--compression", if zlib { "zlib" } else { "none" }.into()));
    out.push(("--sub-charset", "UTF-8".into()));
    out
}

/// The simple tags mkvmerge writes for every track's statistics: left out of what is carried over,
/// since it writes new ones.
pub const STATISTICS: &[&str] = &[
    "BPS",
    "DURATION",
    "NUMBER_OF_FRAMES",
    "NUMBER_OF_BYTES",
    "_STATISTICS_WRITING_APP",
    "_STATISTICS_WRITING_DATE_UTC",
    "_STATISTICS_TAGS",
];

/// The tags of the track with UID `uid` in the Tags element `data` (at `base`), as mkvmerge's tags
/// XML for `--tags` (`None` when there are none, mkvmerge's statistics left out), and what may not
/// carry over exactly. They are carried as text, and nothing checks them afterwards: a value that
/// is not plain UTF-8 text, or a tag that also targets something else, is noted instead.
pub fn track_tags_xml(
    data: &[u8],
    base: u64,
    uid: u64,
) -> ebml::Result<(Option<String>, BTreeSet<&'static str>)> {
    let mut tags = String::new();
    let mut notes = BTreeSet::new();
    for (id, at, tag) in ebml::children(data, base)? {
        if id != mkv::TAG {
            continue;
        }
        let kids = ebml::children(tag, at)?;
        let mut targets = String::new();
        let (mut ours, mut others) = (false, false);
        // Noted only if something of this tag is carried.
        let mut odd = BTreeSet::new();
        for (cid, cat, v) in &kids {
            if *cid != mkv::TARGETS {
                continue;
            }
            for (tid, _, tv) in ebml::children(v, *cat)? {
                match tid {
                    mkv::TAG_TRACK_UID if ebml::uint(tv) == uid => ours = true,
                    mkv::TARGET_TYPE_VALUE => {
                        let _ = write!(
                            targets,
                            "<TargetTypeValue>{}</TargetTypeValue>",
                            ebml::uint(tv)
                        );
                    }
                    mkv::TARGET_TYPE => {
                        let _ = write!(targets, "<TargetType>{}</TargetType>", text(tv, &mut odd));
                    }
                    mkv::CRC32 | mkv::VOID => {}
                    // Another track, an edition, a chapter or an attachment.
                    _ => others = true,
                }
            }
        }
        if !ours {
            continue;
        }
        if others {
            odd.insert("a tag also targets something else");
        }
        let mut simples = String::new();
        for (cid, cat, v) in &kids {
            if *cid == mkv::SIMPLE_TAG {
                simples += &simple_xml(v, *cat, true, &mut odd)?;
            }
        }
        if !simples.is_empty() {
            let _ = writeln!(tags, "<Tag><Targets>{targets}</Targets>{simples}</Tag>");
            notes.extend(odd);
        }
    }
    let xml =
        (!tags.is_empty()).then(|| format!("<?xml version=\"1.0\"?>\n<Tags>\n{tags}</Tags>\n"));
    Ok((xml, notes))
}

/// One SimpleTag as XML; at the `top` level, nothing for one of mkvmerge's statistics. What may
/// not carry exactly goes to `notes` only when it is written.
fn simple_xml(
    data: &[u8],
    base: u64,
    top: bool,
    notes: &mut BTreeSet<&'static str>,
) -> ebml::Result<String> {
    let mut name = String::new();
    let mut rest = String::new();
    let mut odd = BTreeSet::new();
    for (id, at, v) in ebml::children(data, base)? {
        match id {
            mkv::TAG_NAME => name = text(v, &mut odd),
            mkv::TAG_STRING => {
                let _ = write!(rest, "<String>{}</String>", text(v, &mut odd));
            }
            mkv::TAG_BINARY => {
                let _ = write!(rest, "<Binary format=\"hex\">{}</Binary>", hex(v));
            }
            mkv::TAG_LANGUAGE => {
                let _ = write!(rest, "<TagLanguage>{}</TagLanguage>", text(v, &mut odd));
            }
            mkv::TAG_LANGUAGE_BCP47 => {
                let _ = write!(
                    rest,
                    "<TagLanguageIETF>{}</TagLanguageIETF>",
                    text(v, &mut odd)
                );
            }
            mkv::TAG_DEFAULT => {
                let _ = write!(rest, "<DefaultLanguage>{}</DefaultLanguage>", ebml::uint(v));
            }
            mkv::SIMPLE_TAG => rest += &simple_xml(v, at, false, &mut odd)?,
            mkv::CRC32 | mkv::VOID => {}
            _ => {
                odd.insert("a tag has an element mkvmerge's XML cannot hold");
            }
        }
    }
    if top && STATISTICS.contains(&name.as_str()) {
        return Ok(String::new());
    }
    notes.extend(odd);
    Ok(format!("<Simple><Name>{name}</Name>{rest}</Simple>"))
}

/// A string element's value for XML, escaped: up to its first NUL, as Matroska reads a string.
/// Text that XML would not give back as it is (not UTF-8, a CR) is noted.
fn text(v: &[u8], notes: &mut BTreeSet<&'static str>) -> String {
    let v = &v[..v.iter().position(|&b| b == 0).unwrap_or(v.len())];
    match std::str::from_utf8(v) {
        Ok(s) if !s.contains('\r') => xml(s),
        _ => {
            notes.insert("a tag value is not plain UTF-8 text");
            xml(&String::from_utf8_lossy(v))
        }
    }
}

/// Text for XML.
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Run `program` (mkvmerge) with `args`. Its own process group, registered with [`group`], so a
/// signal that ends subcheck ends it too. Ok with mkvmerge's warnings (exit 1), if any; an error
/// with what it said otherwise.
pub fn run(program: &OsStr, args: &[OsString]) -> Result<Option<String>, String> {
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| {
            let name = program.to_string_lossy();
            if e.kind() == io::ErrorKind::NotFound {
                format!("{name} not found: install mkvtoolnix")
            } else {
                format!("starting {name}: {e}")
            }
        })?;
    let _running = group::register(i32::try_from(child.id()).unwrap_or(0));
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for mkvmerge: {e}"))?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .trim()
    .to_string();
    match out.status.code() {
        Some(0) => Ok(None),
        Some(1) => Ok(Some(said)),
        _ => Err(format!("mkvmerge failed ({}): {said}", out.status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (PathBuf, Layout) {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/apply.mkv"
        ));
        let (_, layout) = mkv::open(&path).unwrap();
        (path, layout)
    }

    #[test]
    fn replaced_tracks_keep_their_place_and_properties() {
        let (path, layout) = fixture();
        let replaced = [
            Replacement {
                id: 3,
                file: PathBuf::from("/t/3.ass"),
                tags: Some(PathBuf::from("/t/3.tags.xml")),
            },
            Replacement {
                id: 4,
                file: PathBuf::from("/t/4.srt"),
                tags: None,
            },
        ];
        let a: Vec<String> = args(&path, &layout, &replaced, Path::new("/o/.x.mkv.subcheck"))
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        let uid = hex(layout.segment_uid.as_ref().unwrap());
        let expected = [
            "--quiet",
            "-o",
            "/o/.x.mkv.subcheck",
            "--timestamp-scale",
            "1000000",
            "--segment-uid",
            &uid,
            "--subtitle-tracks",
            "!3,4",
            path.to_str().unwrap(),
            "--language",
            "0:en",
            "--track-name",
            "0:English Signs/Songs",
            "--default-track-flag",
            "0:no",
            "--forced-display-flag",
            "0:yes",
            "--compression",
            "0:zlib",
            "--sub-charset",
            "0:UTF-8",
            "--tags",
            "0:/t/3.tags.xml",
            "/t/3.ass",
            "--language",
            "0:en",
            "--track-name",
            "0:English SDH",
            "--default-track-flag",
            "0:no",
            "--forced-display-flag",
            "0:no",
            "--hearing-impaired-flag",
            "0:yes",
            "--compression",
            "0:none",
            "--sub-charset",
            "0:UTF-8",
            "/t/4.srt",
            "--track-order",
            "0:0,0:1,0:2,1:0,2:0",
        ];
        assert_eq!(a, expected);
    }

    /// One element whose size fits a 1-byte vint.
    fn el(id: &[u8], data: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x80 | u8::try_from(data.len()).unwrap());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn a_tracks_own_tags_become_xml_without_the_statistics() {
        let simple = |name: &[u8], value: &[u8]| {
            el(
                &[0x67, 0xC8],
                &[el(&[0x45, 0xA3], name), el(&[0x44, 0x87], value)].concat(),
            )
        };
        let tag = |targets: &[u8], simples: &[u8]| {
            el(
                &[0x73, 0x73],
                &[el(&[0x63, 0xC0], targets), simples.to_vec()].concat(),
            )
        };
        let track = |uid: u8| el(&[0x63, 0xC5], &[uid]);
        let data = [
            tag(
                &track(7),
                &[simple(b"Source", b"A & <B>"), simple(b"BPS", b"9")].concat(),
            ),
            tag(&track(8), &simple(b"Source", b"other track")),
            tag(&track(9), &simple(b"BPS", b"9")),
        ]
        .concat();
        let (xml, notes) = track_tags_xml(&data, 0, 7).unwrap();
        assert_eq!(
            xml.unwrap(),
            "<?xml version=\"1.0\"?>\n<Tags>\n<Tag><Targets></Targets><Simple><Name>Source</Name>\
             <String>A &amp; &lt;B&gt;</String></Simple></Tag>\n</Tags>\n"
        );
        assert!(notes.is_empty());
        assert_eq!(
            track_tags_xml(&data, 0, 9).unwrap().0,
            None,
            "statistics only"
        );
        assert_eq!(track_tags_xml(&data, 0, 1).unwrap().0, None, "no tags");
    }

    #[test]
    fn what_may_not_carry_exactly_is_noted() {
        let simple = |value: &[u8]| {
            el(
                &[0x67, 0xC8],
                &[el(&[0x45, 0xA3], b"COMMENT"), el(&[0x44, 0x87], value)].concat(),
            )
        };
        let tag = |targets: &[u8], value: &[u8]| {
            el(
                &[0x73, 0x73],
                &[el(&[0x63, 0xC0], targets), simple(value)].concat(),
            )
        };
        let both = [el(&[0x63, 0xC5], &[7]), el(&[0x63, 0xC5], &[8])].concat();
        let latin1 = [b"Caf".as_slice(), &[0xE9]].concat();
        let notes = |data: &[u8]| track_tags_xml(data, 0, 7).unwrap().1;
        assert_eq!(
            notes(&tag(&both, b"two tracks")),
            ["a tag also targets something else"].into()
        );
        assert_eq!(
            notes(&tag(&el(&[0x63, 0xC5], &[7]), &latin1)),
            ["a tag value is not plain UTF-8 text"].into()
        );
        assert_eq!(
            notes(&tag(&el(&[0x63, 0xC5], &[7]), b"one\r\ntwo")),
            ["a tag value is not plain UTF-8 text"].into()
        );
        // NUL padding ends a Matroska string: nothing odd about it, and not carried.
        let padded = tag(&el(&[0x63, 0xC5], &[7]), b"BD\0\0");
        assert_eq!(notes(&padded), [].into());
        let xml = track_tags_xml(&padded, 0, 7).unwrap().0.unwrap();
        assert!(xml.contains("<String>BD</String>"), "{xml}");
        // Nothing of a statistics-only tag is carried, so nothing in it is noted.
        let statistics = el(
            &[0x73, 0x73],
            &[
                el(&[0x63, 0xC0], &both),
                el(
                    &[0x67, 0xC8],
                    &[el(&[0x45, 0xA3], b"BPS"), el(&[0x44, 0x87], &latin1)].concat(),
                ),
            ]
            .concat(),
        );
        assert_eq!(notes(&statistics), [].into());
    }

    /// An executable script `name` in `dir`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        path
    }

    #[test]
    fn mkvmerge_warnings_pass_and_errors_fail() {
        let dir = tempfile::tempdir().unwrap();
        let ok = script(dir.path(), "ok", "exit 0\n");
        assert_eq!(run(ok.as_os_str(), &[]), Ok(None));
        let warns = script(dir.path(), "warns", "echo 'Warning: odd'\nexit 1\n");
        assert_eq!(run(warns.as_os_str(), &[]), Ok(Some("Warning: odd".into())));
        let fails = script(dir.path(), "fails", "echo 'Error: bad' >&2\nexit 2\n");
        let err = run(fails.as_os_str(), &[]).unwrap_err();
        assert!(
            err.starts_with("mkvmerge failed (exit status: 2): Error: bad"),
            "{err}"
        );
        let err = run(dir.path().join("none").as_os_str(), &[]).unwrap_err();
        assert!(err.ends_with("not found: install mkvtoolnix"), "{err}");
    }
}
