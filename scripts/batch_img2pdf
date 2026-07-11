#!/bin/zsh

### Converts directories of images to PDFs

# get name of main directory
maindir=$1
shift

# unzip each zip file in directory
echo "Unzipping every archive in $maindir..."

cd "$maindir"

for zipf in *.zip; do
  unar -q "$zipf" &
done

# wait for all unzips to finish
wait

echo "Done unzipping"

# convert all images in each directory to pdf
echo "Converting directores to pdfs"

dirs=$(find . -not -path '.' -not -path '..' -not -path './zips' -type d)
pdfsdir="../../pdfs/$maindir/chapters"
mkdir -p "$pdfsdir"

for dirname in $dirs; do
  { cd "$dirname"
    images=$(find . -type f -print0 | xargs -0 file --mime-type | grep -F 'image/' | cut -d ':' -f 1 | sort)
    img2pdf $images -o "../$pdfsdir/$dirname.pdf" || echo "$dirname"
    cd ..
  }
done

# wait for all pdf conversions to finish
wait

echo "Done converting all pdfs"

# remove all directories
echo "Removing all directories"
for dirname in $dirs; do
  rm -rf $dirname &
done

wait

echo "Done!"
