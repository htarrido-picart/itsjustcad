import sys, re, subprocess, collections
# self samples -> nearest itsjustcad frame (own addr or ancestor call site) -> atos -i inline chain -> phase
tag = sys.argv[1]; raw = open(f'samp.release.{tag}.txt').read()
base = re.search(r'Load Address: +(0x[0-9a-f]+)', raw).group(1)
g = raw.split('Call graph:')[1].split('Total number in stack')[0]
rx = re.compile(r'^([\s+!:|]*?)(\d+) (\S+)\s+\(in ([^)]+)\)(?:.*?\[(0x[0-9a-fx,.]+)\])?')
nodes = [(len(m[1]), int(m[2]), m[3], m[4], (m[5] or '').split(',')[0]) for l in g.splitlines() if (m := rx.match(l))]
idle = re.compile(r'__psynch_cvwait|__ulock_wait|__workq_kernreturn|mach_msg2_trap|__semwait_signal|swtch_pri|condvar|Condvar|wait_until_cold|sleep|start_wqthread|thread_start')
kid = [0]*len(nodes); st = []; anc = [None]*len(nodes); stk = [None]*len(nodes)
for i, n in enumerate(nodes):
    while st and nodes[st[-1]][0] >= n[0]: st.pop()
    if st: kid[st[-1]] += n[1]
    stk[i] = [nodes[j][4] if nodes[j][4].startswith('0x') else nodes[j][2] for j in st] + [n[4] if n[4].startswith('0x') else n[2]]
    anc[i] = next((j for j in reversed(st) if nodes[j][3] == 'itsjustcad' and nodes[j][4].startswith('0x')), None)
    st.append(i)
self_ = collections.Counter()
for i, (d, n, s, lib, a) in enumerate(nodes):
    k = n - kid[i]
    if k <= 0 or idle.search(s): continue
    j = i if (lib == 'itsjustcad' and a.startswith('0x')) else anc[i]
    self_[(nodes[j][4] if j is not None else None, s if j != i else '', tuple(stk[i]))] += k
addrs = sorted({x for (_, _, sk) in self_ for x in sk if x.startswith('0x')})
out = subprocess.run(['atos', '-i', '-o', 'target/release/itsjustcad', '-l', base] + addrs, capture_output=True, text=True).stdout
chain = dict(zip(addrs, ['|'.join(b.strip().splitlines()) for b in out.strip().split('\n\n')]))
P = [(p, re.compile(r)) for p, r in [l.split('=', 1) for l in open(sys.argv[2]).read().split('\n') if '=' in l]]
cat = collections.Counter(); top = collections.defaultdict(collections.Counter); tot = 0
for (a, leaf, sk), n in self_.items():
    ch = chain.get(a, '?'); tot += n
    full = ' || '.join(chain.get(x, x) for x in reversed(sk))
    p = next((p for p, r in P if r.search(full)), 'other'); cat[p] += n
    top[p][(leaf + ' <- ' if leaf else '') + ch.split('|')[0][:110]] += n
print(f'== {tag} busy={tot} samples(ms CPU)')
for p, n in cat.most_common():
    print(f'{p:12s} {n:6d} {100*n/tot:5.1f}%')
    for k, m in top[p].most_common(int(sys.argv[3]) if len(sys.argv) > 3 else 4): print(f'      {m:6d} {k}')
