import sys, re, subprocess, collections
# self samples per address -> innermost inlined frame (atos -i) -> category
v = sys.argv[1]; prof = sys.argv[2] if len(sys.argv) > 2 else 'release'
raw = open(f'samp.{prof}.{v}.txt').read()
base = re.search(r'(0x[0-9a-f]+) -\s+0x[0-9a-f]+ \+itsjustcad ', raw).group(1)
g = raw.split('Call graph:')[1].split('Total number in stack')[0]
rx = re.compile(r'^([\s+!:|]*?)(\d+) (\S+)\s+\(in ([^)]+)\)(?:.*?\[(0x[0-9a-fx,.]+)\])?')
nodes = []
for l in g.splitlines():
    m = rx.match(l)
    if m: nodes.append((len(m.group(1)), int(m.group(2)), m.group(3), m.group(4), m.group(5)))
kid = [0] * len(nodes); st = []
for i, n in enumerate(nodes):
    while st and nodes[st[-1]][0] >= n[0]: st.pop()
    if st: kid[st[-1]] += n[1]
    st.append(i)
idle = re.compile(r'__psynch_cvwait|__ulock_wait|__workq_kernreturn|mach_msg2_trap|__semwait_signal|swtch_pri|condvar|Condvar|start_wqthread|thread_start|wait_until_cold|sleep')
selfc = collections.Counter(); merged = [0]
for i, (d, n, s, lib, a) in enumerate(nodes):
    k = n - kid[i]
    if k > 0 and not idle.search(s):
        al = [x for x in (a or '').split(',') if x.startswith('0x')] or [None]
        if len(al) > 1 or (a and '...' in a): merged[0] += k  # sample merged sibling addresses: split self evenly (approximate)
        for x in al: selfc[(s, lib, x)] += k / len(al)
addrs = sorted({a for (s, lib, a) in selfc if lib == 'itsjustcad' and a})
out = subprocess.run(['atos', '-i', '-o', f'target/{prof}/itsjustcad', '-l', base] + addrs, capture_output=True, text=True).stdout
# atos -i: frames per address, innermost first, blank line between addresses
chains = [b.strip().splitlines() for b in out.strip().split('\n\n')]
inner = dict(zip(addrs, [c[0] for c in chains])); chain = dict(zip(addrs, ['|'.join(c) for c in chains]))
BUILD = re.compile(r'sah_split|half_area|collapse|build_node|partition|select_nth|append_subtree|build|sort|median')
TRAV = re.compile(r'traverse|ray_any_hit|ray_closest_hit|ray_occluded|ray_hit|ray_aabb|node_bounds|NodeStack|ray_candidates|ray_test|Ray32|any_hit_tally|closest_hit_tally|leaf_prims|splat4|f32_down|f32_up|f32_around|beyond|wide::')
cat = collections.Counter(); other = collections.Counter(); tot = 0
for (s, lib, a), n in selfc.items():
    tot += n
    f = inner.get(a, s + ' (in ' + lib + ')') if lib == 'itsjustcad' else s + ' (in ' + lib + ')'
    ch = chain.get(a, s) if lib == 'itsjustcad' else s
    if 'ray_triangle' in ch: c = 'triangle'
    elif TRAV.search(ch): c = 'traversal'
    elif BUILD.search(ch) and ('bvh' in ch): c = 'bvh-build'
    else: c = 'other'; other[s] += n
    cat[c] += n
print(f'== {prof} {v} busy={tot:.0f} merged-addr={100*merged[0]/tot:.1f}%', ' '.join(f'{c}={100*n/tot:.1f}%' for c, n in cat.most_common()))
dm = subprocess.run(['rustfilt'], input='\n'.join(k for k, _ in other.most_common(10)), capture_output=True, text=True).stdout.splitlines()
for (f, n), d in zip(other.most_common(10), dm): print(f'  {100*n/tot:5.1f}% {d[:150]}')
if len(sys.argv) > 3:  # debug: inlined chains of the top unclassified itsjustcad addresses
    oc = collections.Counter()
    for (s, lib, a), n in selfc.items():
        ch = chain.get(a)
        if ch and 'ray_triangle' not in ch and not TRAV.search(ch) and not (BUILD.search(ch) and 'bvh' in ch): oc[ch] += n
    for ch, n in oc.most_common(int(sys.argv[3])): print(f'  {100*n/tot:5.1f}% {ch[:400]}')
