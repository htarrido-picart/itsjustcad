#!/bin/zsh
# usage: run.sh <tag> <script> <n> [bin]  (env ITSJUSTCAD_HWRT passes through) -> wall, cpu, cores, load
S=${0:A:h}; B=${4:-$S/target/release/itsjustcad}
for i in $(seq 1 $3); do
  L=$(uptime | sed 's/.*averages: //')
  /usr/bin/time -p $B --open $S/site.json --run $S/$2.script --headless > $S/out.$1.$2.txt 2> $S/time.$1.tmp
  awk -v v=$2 -v p=$1 -v l="$L" '/^real/{r=$2}/^user/{u=$2}/^sys/{s=$2}END{printf "%s %s real=%.2f cpu=%.2f cores=%.2f load=%s\n",p,v,r,u+s,(u+s)/r,l}' $S/time.$1.tmp
done
