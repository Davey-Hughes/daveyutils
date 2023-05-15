#!/bin/bash

find . -iname "*.mkv" -print0 | while read -d $'\0' name; do
  epname="$(basename "$(echo $name | cut -d'-' -f3 | awk '{$1=$1};1')" .mkv)";
  echo $epname;
  mkvpropedit "$name" -e info --set "title=$epname";
done
