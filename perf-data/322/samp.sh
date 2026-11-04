#!/bin/zsh
# usage: samp.sh <tag> <script> -> samp.<tag>.txt (whole process, 1 ms)
S=${0:A:h}; B=$S/target/release/itsjustcad
$B --open $S/site.json --run $S/$2.script --headless > $S/out.samp.release.$1.txt 2>&1 &
P=$!; /usr/bin/sample $P 30 1 -mayDie -file $S/samp.release.$1.txt >/dev/null 2>&1; wait $P
