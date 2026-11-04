# Raw data for #314 (not for merge)

Re-measure of `radiation`, `facesunhours`, `raytrace`, `sunhours` at `97f77c7` (4-wide f32 BVH #312, SAH #311, tie rule #310, uuid fast-rng #305). Apple M6, 12 cores. Fixture: `site.json` from #286/#302 (569,116 triangles; not committed).

- `times.release.txt`: `[profile.release]` (opt-level "s", fat LTO), built with `CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`. 3 runs per verb, with the 1/5/15-min load per run.
- `times.o3.txt`: the same, plus `o3-cargo.diff` (`opt-level = 3` for kernel-mesh, itsjustcad-raytrace, itsjustcad-solar, itsjustcad-commands). That edit was never committed.
- `samp.release.*.txt.gz`: `/usr/bin/sample <pid> 30 1` over the whole process (`samp.sh`). Load at sample time is in `samp-load.txt`.
- `cls.txt`: output of `classify.py <verb> release 8` (`atos -i` inline attribution, with the top unclassified inline chains).
- `run.sh`, `samp.sh`, `*.script`, `synth.epw`: harness. Scripts hold absolute scratch paths.
- `rt.png`: the `raytrace` output. It is byte-identical to #302's and to the opt-level 3 build's.

`classify.py` change vs #302: the address regex now accepts `[0xA,0xB,...]`. That is a call-graph node where `sample` merged sibling addresses. #302's regex dropped those addresses and classified the node by its symbol name. For `raytrace`, the merged node is the rayon render helper, so #302 put those samples in "other". Now the self samples of a merged node are split evenly across the addresses it lists. This is approximate: the list is truncated.
