import sys, re, subprocess, collections
# self samples per (symbol, file:line) from sample call graph
v = sys.argv[1]; filt = sys.argv[2] if len(sys.argv) > 2 else None
t = subprocess.run(['rustfilt'], stdin=open(f'samp.{v}.txt'), capture_output=True, text=True).stdout
g = t.split('Call graph:')[1].split('Total number in stack')[0]
rx = re.compile(r'^([\s+!:|]*?)(\d+) (.*?)\s+\(in ([^)]+)\)(?:.*?\[(0x[0-9a-f]+)\]\s+(\S+))?\s*$')
nodes = []
for l in g.splitlines():
    m = rx.match(l)
    if m: nodes.append([len(m.group(1)), int(m.group(2)), m.group(3), (m.group(6) or '?') + ' ' + (m.group(5) or '')])
self_ = collections.Counter(); kid = [0]*len(nodes); st = []
for i, (d, n, s_, loc) in enumerate(nodes):
    while st and nodes[st[-1]][0] >= d: st.pop()
    if st: kid[st[-1]] += n
    st.append(i)
for i, (d, n, s_, loc) in enumerate(nodes): self_[(s_, loc)] += n - kid[i]
idle = ('__psynch_cvwait', '__ulock_wait', '__workq_kernreturn', 'mach_msg2_trap', '__semwait_signal', 'swtch_pri', '<std::sync::poison::condvar', 'std::sys::sync::condvar', 'start_wqthread', 'thread_start')
busy = {k: n for k, n in self_.items() if n > 0 and not k[0].startswith(idle)}
tot = sum(busy.values()); print(f'== {v} busy={tot}')
cat = collections.Counter()
for (s, loc), n in busy.items():
    if filt and filt not in s: continue
    cat[(s[:70], loc)] += n
for k, n in cat.most_common(int(sys.argv[3]) if len(sys.argv) > 3 else 25): print(f'{100*n/tot:5.1f}% {n:6d} {k[0]}  {k[1]}')
