//! A tiny case-insensitive subsequence fuzzy matcher for the pane picker.

/// Score `candidate` against `query` as a case-insensitive subsequence:
/// `Some(score)` if every char of `query` appears in `candidate` in order
/// (not necessarily adjacent), else `None`. Higher is better — contiguous runs
/// and matches at a word start (string start or after a separator) score more.
/// An empty query matches everything with score 0.
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = candidate.to_lowercase().chars().collect();
    let mut qi = 0usize;
    let mut total = 0i64;
    let mut prev_matched = false;
    for (i, &ch) in c.iter().enumerate() {
        if qi < q.len() && ch == q[qi] {
            total += 1;
            if prev_matched {
                total += 2; // contiguous run
            }
            let at_word_start = i == 0 || matches!(c[i - 1], ' ' | ':' | '.' | '-' | '_' | '/');
            if at_word_start {
                total += 3;
            }
            qi += 1;
            prev_matched = true;
        } else {
            prev_matched = false;
        }
    }
    (qi == q.len()).then_some(total)
}

/// Indices of `candidates` matching `query`, best score first; stable within
/// equal scores (original order preserved). Empty query → every index in order.
pub fn filter(query: &str, candidates: &[String]) -> Vec<usize> {
    let mut scored: Vec<(usize, i64)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, cand)| score(query, cand).map(|s| (i, s)))
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matches_out_of_order_chars() {
        assert!(
            score("cld", "claude").is_some(),
            "cld is a subsequence of claude"
        );
        assert!(score("cl", "claude").is_some());
        assert!(score("xyz", "claude").is_none());
        assert_eq!(score("", "anything"), Some(0), "empty query matches");
    }

    #[test]
    fn case_insensitive() {
        assert!(score("CLD", "claude").is_some());
        assert!(score("cld", "CLAUDE").is_some());
    }

    #[test]
    fn a_word_start_match_outranks_a_mid_word_match() {
        // "c" at the start of "claude" should score higher than "c" buried in "vac".
        assert!(score("c", "claude").unwrap() > score("c", "vac").unwrap());
    }

    #[test]
    fn filter_keeps_only_matches_best_first() {
        let cands: Vec<String> = ["bot:0.1 claude", "work:1.2 vim", "side:0.0 clock"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let out = filter("cl", &cands);
        assert!(out.contains(&0), "claude matches");
        assert!(out.contains(&2), "clock matches");
        assert!(!out.contains(&1), "vim does not match cl");
    }

    #[test]
    fn empty_query_lists_all_in_order() {
        let cands: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(filter("", &cands), vec![0, 1, 2]);
    }
}
