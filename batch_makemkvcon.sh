#!/bin/bash

find . -iname "*index.bdmv" ! -path "*/BACKUP/*" -print0 | while read -d $'\0' file; do 
  outpath="/media/media 0/needs_processing/converted blu-ray discs/$(echo $file | cut -d'/' -f2)"; 
  mkdir "$outpath"; 
  makemkvcon mkv file:"$file" all "$outpath" --minlength=0 --robot; 
done
