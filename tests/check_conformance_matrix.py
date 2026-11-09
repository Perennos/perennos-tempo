"""Check the published traceability contract, not legal compliance."""
from pathlib import Path
from collections import Counter
import json
import re
import sys

repo = Path(sys.argv[1])
data = json.loads((repo / "docs/CONFORMANCE-REQUIREMENTS.json").read_text(encoding="utf-8"))
rows = data["requirements"]
ids = [r["id"] for r in rows]
assert len(ids) == len(set(ids))
clock = {letter+str(n) for letter, count in zip("ABCDEFGHI", [4,4,4,5,3,3,4,5,1]) for n in range(1,count+1)}
assert clock <= set(ids)
assert len([r for r in rows if r["id"].startswith("RED")]) == 5
assert len([r for r in rows if re.fullmatch(r"P[1-7]",r["id"])]) == 7
assert len([r for r in rows if r["section"].startswith("8.")]) == 63
assert len([r for r in rows if r["section"] == "14"]) == 10
assert len([r for r in rows if r["section"] == "15"]) == 4
assert len([r for r in rows if r["section"] == "Part Two"]) == 7
anchors = set()
for row in rows:
    assert all(row[k] for k in ["requirement","existing","missing","evidence_path","proof"])
    assert row["status"] == "In progress"
    anchors.update(re.findall(chr(96)+r"([^`]+:\d+)"+chr(96),row["existing"]))
    for path in row['evidence_path'].split('; '):
        assert re.fullmatch(r'conformance/CASE/(?:use-case|data-flow|models|signoffs|incidents-overrides|evaluation-canary|signed-receipts)\.json#[a-z_]+',path), (row['id'],path)
by_id={row['id']:row for row in rows}
assert 'use-case.json#contract_reference' in by_id['S5.3-04']['evidence_path']
assert 'use-case.json#outsourcing_reference' in by_id['S5.3-06']['evidence_path']
assert 'use-case.json#lawful_basis_reference' in by_id['C2']['evidence_path']
assert 'use-case.json#stop_test_reference' in by_id['H2']['evidence_path']
assert 'use-case.json#contract_reference' in by_id['D2']['evidence_path']
assert 'use-case.json#appeal_reference' in by_id['P3']['evidence_path']
assert 'use-case.json#explanation_reference' in by_id['P3']['evidence_path']
for rid in ['S5.3-08','CLK-GATE']:
    assert len(by_id[rid]['evidence_path'].split('; '))==8
    assert 'before the first real record' in by_id[rid]['proof']
    assert 'A1-I1' in by_id[rid]['proof']
assert by_id['S14-08']['evidence_kind']=='signoffs'
assert by_id['S14-09']['evidence_kind']=='signoffs'
assert by_id['P6']['evidence_kind']=='signed-receipts'
assert 'No native register' in by_id['P1']['existing']
matrix=(repo/'docs/CONFORMANCE-MATRIX.md').read_text(encoding='utf-8')
published=[line for line in matrix.splitlines() if re.match(r'\| [A-Z0-9][^|]* / section',line)]
expected=[f"| {r['id']} / section{r['section']}, text line{r['source_line']} | {r['requirement']} | {r['existing']} | {r['missing']} | `{r['evidence_path']}`. {r['proof']} |" for r in rows]
assert published==expected, 'Markdown/JSON traceability mismatch'
for anchor in anchors:
    name, line = anchor.rsplit(":",1)
    path=repo/name
    assert path.is_file(), anchor
    assert 0<int(line)<=len(path.read_text(encoding="utf-8-sig").splitlines()), anchor
for p in (repo/"docs").glob("CONFORMANCE*.md"):
    assert not re.search(r"\b(?:ledger|claim)\b",p.read_text(encoding="utf-8"),re.I), p.name
print(json.dumps({"rows":len(rows),"clock_fields":len(clock),"code_anchors":len(anchors),"missing_evidence_paths":0,"duplicate_ids":0,"checks":"passed"},sort_keys=True))
