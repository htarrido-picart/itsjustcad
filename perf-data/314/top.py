import sys,re,subprocess
for v in sys.argv[1:]:
    t=subprocess.run(['rustfilt'],stdin=open(f'samp.{v}.txt'),capture_output=True,text=True).stdout
    sec=t.split('Sort by top of stack')[1].split('Binary Images')[0]
    rows=[]
    for l in sec.splitlines():
        m=re.match(r'\s+(.*?)\s+\(in (\S+)\)\s+(\d+)\s*$',l)
        if m: rows.append((m.group(1),int(m.group(3))))
    idle={'__psynch_cvwait','__ulock_wait','__workq_kernreturn','mach_msg2_trap','__semwait_signal','swtch_pri'}
    busy=[r for r in rows if r[0] not in idle]; tot=sum(n for _,n in busy)
    print(f'== {v} busy={tot}')
    for f,n in busy[:14]: print(f'  {100*n/tot:5.1f}% {f[:120]}')
