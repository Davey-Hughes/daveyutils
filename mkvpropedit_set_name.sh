#!/bin/bash

find . -iname "*.mkv" -print0 | while read -d $'\0' name; do
  epname="$(basename -s .mkv $name | cut -d'-' -f3 | awk '{$1=$1};1')";
  echo $epname;
  mkvpropedit "$name" -e info --set "title=$epname";
done
