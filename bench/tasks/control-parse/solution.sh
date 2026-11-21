#!/bin/sh
# Reference solution, applied to the repo. Used only by --validate.
set -e
node -e 'const fs = require("fs"); fs.writeFileSync("parse.js", fs.readFileSync("parse.js", "utf8").replace(/\btmp\b/g, "fields"));'
