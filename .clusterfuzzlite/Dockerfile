FROM gcr.io/oss-fuzz-base/base-builder-rust AS builder

COPY . $SRC/ccag
WORKDIR $SRC/ccag

COPY .clusterfuzzlite/build.sh $SRC/build.sh
