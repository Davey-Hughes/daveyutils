#!/bin/zsh

### Bisect landscape orientation images and save both files

find . -name '*.jpg' -print0 | while read -d $'\0' f; do
  out="$(basename "$(dirname $f)")_"$(basename $f)""
  out="${out%.*}"
  aspect="$(convert $f -format '%[fx:w/h]' info:)"
  if [[ $aspect -gt 1 ]]; then
    # cp "$f" "./$out-base.jpg"
    imgformat="$(convert $f -format '%m' info:)"

    # if [[ $imgformat == "JPEG" ]]; then
    if [[ "1" == "2" ]]; then
      width="$(convert $f -format '%[fx:w]' info:)"
      leftwidth=$(( width / 2 ))
      rightwidth=$(( width - leftwidth ))
      sem -j+0 "$(jpegtran -perfect -crop $leftwidth -outfile "$out-1.jpg" "$f" && jpegtran -perfect -crop $rightwidth+$leftwidth -outfile "$out-0.jpg" "$f")"

    else
      sem -j+0 "$(convert -crop 50%x100% +repage -quality 100 "$f" "$out-%d.jpg" && mv "$out-0.jpg" "$out-2.jpg")"
    fi

  fi
done
