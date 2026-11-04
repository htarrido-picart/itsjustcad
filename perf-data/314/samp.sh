#!/bin/zsh
# usage: samp.sh <profile> <verb>  -> samp.<profile>.<verb>.txt (whole process, 30 s max, 1 ms)
S=${0:A:h}; B=$S/target/$1/itsjustcad
$B --open $S/site.json --run $S/$2.script --headless > $S/out.samp.$1.$2.txt 2>&1 &
P=$!; /usr/bin/sample $P 30 1 -mayDie -file $S/samp.$1.$2.txt >/dev/null 2>&1; wait $P
