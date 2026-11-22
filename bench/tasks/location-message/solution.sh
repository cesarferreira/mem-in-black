#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
sed -i.bak "s/^negative=.*/negative=Amount must be zero or more./" i18n/source.txt && rm i18n/source.txt.bak
node i18n/build.js
