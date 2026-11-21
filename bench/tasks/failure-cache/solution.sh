#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
sed -i.bak 's/CACHE_VERSION = 1/CACHE_VERSION = 2/' cache.js && rm cache.js.bak
