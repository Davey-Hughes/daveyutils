#! /bin/fish

# USAGE: `printf "%b" "$COLOR_B\e0This is a blue text.$COLOR_RESET"`
# printf "%b" "$BG_R\e0This is text with a red background.$COLOR_RESET"
set COLOR_RESET "\033[0m"

set COLOR_K "\033[0;30m" # black
set COLOR_R "\033[0;31m" # red
set COLOR_G "\033[0;32m" # green
set COLOR_Y "\033[0;33m" # yellow
set COLOR_B "\033[0;34m" # blue
set COLOR_M "\033[0;35m" # magenta
set COLOR_C "\033[0;36m" # cyan
set COLOR_W "\033[0;37m" # white

# empahsized (bolded) colors
set EM_K "\033[1;30m"
set EM_R "\033[1;31m"
set EM_G "\033[1;32m"
set EM_Y "\033[1;33m"
set EM_B "\033[1;34m"
set EM_M "\033[1;35m"
set EM_C "\033[1;36m"
set EM_W "\033[1;37m"

# background colors
set BG_K "\033[40m"
set BG_R "\033[41m"
set BG_G "\033[42m"
set BG_Y "\033[43m"
set BG_B "\033[44m"
set BG_M "\033[45m"
set BG_C "\033[46m"
set BG_W "\033[47m"

# usage: color_print $COLOR_R "I am red"
# multiline: color_print $COLOR_R "I am red\n"; color_print $COLOR_B "I am blue"
function color_print
    printf "%b" "$argv[1]\e0$argv[2]$COLOR_RESET"
end

function inc -a int
    math $int + 1
end

argparse dry_run only_output copy_first "g/glob=" "o/output_dir=" "m#max_files" -- $argv
or exit

if not set -q _flag_glob
    set _flag_glob "*.mkv"
end

if not set -q _flag_output_dir
    set _flag_output_dir "/mnt/daveynet/nfs/media 0/needs_processing/ffmpeg_output/"
end

set error_files

set i 0
fd --ignore-case --glob $_flag_glob -0 | while read -z input
    # break if max files is reached
    if set -q _flag_max_files
        if test $i -ge $_flag_max_files
            break
        end
    end
    set i (math $i + 1)

    if not set -q _flag_only_output
        color_print $EM_Y $input\n
    end

    set audio_track_info (ffprobe -v error -select_streams a -show_entries stream=index,codec_name:stream_tags=title -of csv=print_section=0 "$input")

    set audio_streams
    set metadata

    for line in $audio_track_info
        # get stream index, codec name, and title
        set stream (string split , $line)

        # if stream title is set and stream type is PCM
        if string match --quiet --all --regex '^pcm_.*' $stream[2]

            # replace "PCM" with "FLAC" in stream title if title exists
            if set -q stream[3]
                set stream[3] (string replace PCM FLAC $stream[3])
                set -a metadata -metadata:s:$stream[1] title="$stream[3]"
            end

            # set output stream to encode to flac
            set -a audio_streams -c:$stream[1] flac
        end
    end

    if not set -q audio_streams[1]
        if not set -q _flag_only_output
            color_print $EM_R \t"No PCM audio streams found in "
            color_print $COLOR_Y $input\n
            echo
        end
    else
        set show_name (basename $PWD)
        set output (realpath -m "$_flag_output_dir/$show_name/$input")

        if set -q _flag_copy_first
            function ffmpeg_command
                ffmpeg -n -nostdin -hide_banner -v error -stats -i "$input" -map 0 -c copy -f matroska pipe: | ffmpeg -n -hide_banner -v error -stats -f matroska -i pipe: -map 0 $metadata -c copy $audio_streams "$output"
            end
        else
            function ffmpeg_command
                ffmpeg -n -nostdin -hide_banner -v error -stats -i "$input" -map 0 $metadata -c copy $audio_streams "$output"
            end
        end

        if not set -q _flag_only_output
            printf \t
        end

        printf "$input"\n

        if not set -q _flag_dry_run
            if test -e "$output"
                color_print $EM_R \t"Output file already exists, skipping: "
                color_print $COLOR_Y $output\n
            else
                mkdir -p (dirname "$output")
                ffmpeg_command
                set ffmpeg_status $status

                # ffmpeg returned 0
                set ffmpeg_status_color
                if [ $status = 0 ]
                    set ffmpeg_status_color $COLOR_G
                else
                    set ffmpeg_status_color $EM_R
                    rm "$output"
                    set -a error_files $input
                end

                color_print $ffmpeg_status_color \t"status: "$ffmpeg_status\n\n

                chmod -R oug+rw "$_flag_output_dir" &
            end
        end

        if not set -q _flag_only_output
            echo
        end
    end
end

if set -q error_files[1]
    color_print $EM_R "Errored files:"\n
    printf %s\n $error_files
end
