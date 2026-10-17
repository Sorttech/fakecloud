#!/usr/bin/env python3
"""Regenerate crates/fakecloud-sagemaker/src/generated.rs from aws-models/sagemaker.json.

Amazon SageMaker is an awsJson1.1 service with ~404 operations; hand-maintaining
the operation table, per-operation input constraints, output member shapes, list
element shapes, resource families and identifier members would be error-prone, so
they are generated directly from the Smithy model. Unlike the restJson1 IoT crate,
SageMaker routes every request by its `X-Amz-Target: SageMaker.<Operation>` header
and carries *all* inputs in the JSON body (no HTTP path/label/query bindings), so
the generated metadata is simpler: an operation verb, its resource family, the
identifier member used as the storage key, and the validation / projection shapes.

Run after refreshing aws-models/sagemaker.json:

    python3 scripts/generate-sagemaker-tables.py
"""
import json, os, re

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
M = os.path.join(REPO, "aws-models", "sagemaker.json")
d = json.load(open(M))
shapes = d["shapes"]
NS = "com.amazonaws.sagemaker#"


def sn(t):
    return t.split("#")[1]


def resolve(tid):
    return shapes.get(tid)


def kind_of(tid):
    s = resolve(tid)
    if not s:
        p = tid.split("#")[1] if "#" in tid else tid
        if p in ("String", "Document"):
            return "Str"
        if p in ("Integer", "Long", "Short", "Byte", "BigInteger",
                 "PrimitiveInteger", "PrimitiveLong"):
            return "Int"
        if p in ("Float", "Double", "BigDecimal", "PrimitiveFloat",
                 "PrimitiveDouble"):
            return "Num"
        if p in ("Boolean", "PrimitiveBoolean"):
            return "Bool"
        if p == "Blob":
            return "Blob"
        if p == "Timestamp":
            return "Ts"
        return "Str"
    t = s["type"]
    if t == "structure":
        return "Struct"
    if t == "list":
        return "List"
    if t in ("map",):
        return "Map"
    if t == "union":
        return "Struct"
    if t == "enum":
        return "Str"
    if t == "intEnum":
        return "Int"
    if t == "string":
        return "Str"
    if t in ("integer", "long", "short", "byte", "biginteger"):
        return "Int"
    if t in ("float", "double", "bigdecimal"):
        return "Num"
    if t == "boolean":
        return "Bool"
    if t == "blob":
        return "Blob"
    if t == "timestamp":
        return "Ts"
    if t == "document":
        return "Str"
    return "Str"


def enum_values(tid):
    s = resolve(tid)
    if not s:
        return None
    if s["type"] in ("enum", "intEnum"):
        vals = []
        for mn, mm in s.get("members", {}).items():
            ev = mm.get("traits", {}).get("smithy.api#enumValue", mn)
            vals.append(ev)
        return vals
    et = s.get("traits", {}).get("smithy.api#enum")
    if et:
        return [e["value"] for e in et]
    return None


def constraints(tid, mtr):
    s = resolve(tid)
    st = (s or {}).get("traits", {})

    def pick(k):
        if k in mtr:
            return mtr[k]
        return st.get(k)

    ln = pick("smithy.api#length") or {}
    rg = pick("smithy.api#range") or {}
    return ln.get("min"), ln.get("max"), rg.get("min"), rg.get("max")


def struct_members(tid):
    s = resolve(tid)
    if not s or s["type"] not in ("structure", "union"):
        return []
    return list(s.get("members", {}).items())


def list_length_min(tid):
    """The @length `min` of a list shape, if any."""
    s = resolve(tid)
    if not s or s["type"] != "list":
        return None
    return (s.get("traits", {}).get("smithy.api#length", {}) or {}).get("min")


# Maximum recursion depth when expanding the required-output-member tree. The
# SageMaker model nests required structures at most ~4 levels; the cap plus the
# on-path cycle guard keeps generation terminating on self-referential shapes.
MAX_REQ_DEPTH = 6


def is_union(tid):
    s = resolve(tid)
    return bool(s and s["type"] == "union")


def field_for(mn, m, path, depth):
    """Describe a single member `mn` (with member shape `m`) as a field dict.

    For a structure member, `children` holds its own required members. For a
    union member, `is_union` is set and `children` holds exactly its first member
    (so a synthesised union carries exactly one member). For a list member,
    `elem_kind` is the element kind, `list_min` the list's `@length` min, and
    (for a structure element) `children` the element's required members."""
    mtr = m.get("traits", {})
    ctid = m["target"]
    kn = kind_of(ctid)
    mn_min, _mx, rmin, _rmx = constraints(ctid, mtr)
    evs = enum_values(ctid) if kn == "Str" else None
    fld = {
        "wire": mn, "kind": kn, "enums": evs,
        "min_len": mn_min, "min_val": rmin,
        "list_min": None, "elem_kind": "Str", "children": [],
        "is_union": False,
    }
    if kn == "Struct" and is_union(ctid):
        fld["is_union"] = True
        # Populate exactly the first union member so the value is a valid union.
        mems = struct_members(ctid)
        if mems and ctid not in path and depth <= MAX_REQ_DEPTH:
            fmn, fm = mems[0]
            fld["children"] = [field_for(fmn, fm, path + [ctid], depth + 1)]
    elif kn == "Struct" and ctid not in path:
        fld["children"] = required_tree(ctid, path + [ctid], depth + 1)
    elif kn == "List":
        fld["list_min"] = list_length_min(ctid)
        ls = resolve(ctid)
        elt = ls.get("member", {}).get("target") if ls else None
        ek = kind_of(elt) if elt else "Str"
        fld["elem_kind"] = ek
        if ek == "Struct" and elt and elt not in path:
            fld["children"] = required_tree(elt, path + [elt], depth + 1)
        elif ek == "Str" and elt:
            # A scalar (possibly enum) element: reuse `enums` to carry the
            # element's valid values so a synthesised element is valid.
            fld["enums"] = enum_values(elt)
    return fld


def required_tree(tid, path, depth):
    """Recursively describe every `@required` member of structure `tid`.

    Recursion stops at `MAX_REQ_DEPTH` or when a shape reappears on the current
    ancestor path (cycle guard)."""
    s = resolve(tid)
    if not s or s["type"] not in ("structure", "union") or depth > MAX_REQ_DEPTH:
        return []
    fields = []
    for mn, m in struct_members(tid):
        if "smithy.api#required" not in m.get("traits", {}):
            continue
        fields.append(field_for(mn, m, path, depth))
    return fields


VERBS = ["Create", "Describe", "Delete", "List", "Update", "Get", "Stop",
         "Start", "Register", "Deregister", "Add", "Batch", "Search", "Send",
         "Retry", "Import", "Query", "Enable", "Disable", "Associate",
         "Disassociate", "Put", "Attach", "Detach", "Extend", "Render"]


def singular(b):
    if b.endswith("ies"):
        return b[:-3] + "y"
    if b.endswith("sses"):
        return b[:-2]
    if b.endswith("s") and not b.endswith("ss"):
        return b[:-1]
    return b


def family_of(name):
    for v in VERBS:
        if name.startswith(v):
            base = name[len(v):]
            if v == "List":
                base = singular(base)
            return v, base
    return "OTHER", name


def verb_of(name):
    if name.startswith("Create"):
        return "Create"
    if name.startswith("Describe"):
        return "Get"
    if name.startswith("List"):
        return "List"
    if name.startswith("Update"):
        return "Update"
    if name.startswith("Delete"):
        return "Delete"
    return "Action"


def kebab(fam):
    s = re.sub(r"(?<!^)(?=[A-Z])", "-", fam).lower()
    return s


def input_member_names(inp):
    return [mn for mn, _ in struct_members(inp)]


def key_member_of(name, fam, inp):
    mems = input_member_names(inp)
    for cand in (fam + "Name", fam + "Id", fam + "Arn", fam, fam + "JobName"):
        if cand in mems:
            return cand
    # first required string member
    for mn, m in struct_members(inp):
        if "smithy.api#required" in m.get("traits", {}) and kind_of(m["target"]) == "Str":
            return mn
    if mems:
        return mems[0]
    return ""


ops = sorted([sn(k) for k, v in shapes.items() if v.get("type") == "operation"])
meta = {}
for name in ops:
    o = shapes[NS + name]
    verb = verb_of(name)
    _v, base = family_of(name)
    fam = base if verb != "List" else base
    inp = o.get("input", {}).get("target")
    out = o.get("output", {}).get("target")
    if inp and sn(inp) == "Unit":
        inp = None
    if out and sn(out) == "Unit":
        out = None
    errs = [sn(e["target"]) for e in o.get("errors", [])]

    key_member = ""
    if verb in ("Create", "Get", "Update", "Delete") and inp:
        key_member = key_member_of(name, fam, inp)

    # validation rules: required / constrained / enum body members
    rules = []
    if inp:
        for mn, m in struct_members(inp):
            mtr = m.get("traits", {})
            req = "smithy.api#required" in mtr
            tid = m["target"]
            kn = kind_of(tid)
            mn_min, mn_max, rmin, rmax = constraints(tid, mtr)
            evs = enum_values(tid) if kn == "Str" else None
            if not (req or mn_min is not None or mn_max is not None
                    or rmin is not None or rmax is not None or evs):
                continue
            rules.append({"wire": mn, "req": req, "kind": kn,
                          "min_len": mn_min, "max_len": mn_max,
                          "min_val": rmin, "max_val": rmax, "enums": evs})

    # output members (all body members for awsJson)
    omembers = []
    if out:
        for mn, m in struct_members(out):
            omembers.append({"wire": mn, "kind": kind_of(m["target"])})

    # first list-typed output member -> list projection
    list_elem = None
    list_scalar = False
    req_elem = []
    if out:
        for mn, m in struct_members(out):
            if kind_of(m["target"]) == "List":
                ls = resolve(m["target"])
                elt = ls.get("member", {}).get("target") if ls else None
                ek = kind_of(elt) if elt else "Struct"
                if ek in ("Str", "Int", "Num", "Bool", "Blob", "Ts"):
                    list_scalar = True
                elems = []
                if elt:
                    for emn, em in struct_members(elt):
                        elems.append({"wire": emn, "kind": kind_of(em["target"])})
                list_elem = {"wire": mn, "elems": elems}
                # Required members the projected list element must carry.
                if elt and ek == "Struct":
                    req_elem = required_tree(elt, [out, elt], 2)
                break

    # Full required-output-member tree (recursive): every @required output
    # member, and for structure/list members their required sub-members, so the
    # engine can synthesise a shape-valid value for each server-derived member.
    req_out = required_tree(out, [out], 1) if out else []

    meta[name] = {
        "verb": verb, "family": fam, "key_member": key_member,
        "arn_path": kebab(fam) if fam else "",
        "errors": errs, "rules": rules, "omembers": omembers,
        "list_elem": list_elem, "list_scalar": list_scalar,
        "req_out": req_out, "req_elem": req_elem,
        "has_input": bool(inp),
    }

print("ops:", len(meta))
from collections import Counter
print(Counter(m["verb"] for m in meta.values()))


def rs(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def opt_i(x):
    return "None" if x is None else f"Some({int(x)})"


def kind(k):
    return f"K::{k}"


def render_fields(fields):
    """Render a list of required-field dicts as a Rust `&[Field]` literal."""
    if not fields:
        return "&[]"
    parts = []
    for f in fields:
        enums = "&[" + ", ".join(rs(e) for e in (f.get("enums") or [])) + "]"
        parts.append(
            "Field { wire: %s, kind: %s, enums: %s, min_len: %s, min_val: %s, list_min: %s, elem_kind: %s, is_union: %s, children: %s }" % (
                rs(f["wire"]), kind(f["kind"]), enums,
                opt_i(f.get("min_len")), opt_i(f.get("min_val")),
                opt_i(f.get("list_min")), kind(f.get("elem_kind", "Str")),
                "true" if f.get("is_union") else "false",
                render_fields(f.get("children") or [])))
    return "&[" + ", ".join(parts) + "]"


verbmap = {"Create": "Create", "Get": "Get", "List": "List",
           "Update": "Update", "Delete": "Delete", "Action": "Action"}

out = []
out.append("// @generated by scripts/generate-sagemaker-tables.py (do not edit by hand)")
out.append("// Amazon SageMaker (sagemaker) awsJson1.1 operation metadata derived from the Smithy model.")
out.append("#![allow(clippy::all)]")
out.append("")
out.append("#[derive(Clone, Copy, PartialEq, Eq, Debug)]")
out.append("pub enum K { Str, Int, Num, Bool, Blob, Ts, List, Map, Struct }")
out.append("")
out.append("#[derive(Clone, Copy, PartialEq, Eq, Debug)]")
out.append("pub enum Verb { Create, Get, List, Update, Delete, Action }")
out.append("")
out.append("pub struct Rule {")
out.append("    pub wire: &'static str,")
out.append("    pub req: bool,")
out.append("    pub kind: K,")
out.append("    pub min_len: Option<u64>,")
out.append("    pub max_len: Option<u64>,")
out.append("    pub min_val: Option<i64>,")
out.append("    pub max_val: Option<i64>,")
out.append("    pub enums: &'static [&'static str],")
out.append("}")
out.append("")
out.append("/// A `@required` output member (recursively for structures / list")
out.append("/// elements), enough for the engine to synthesise a shape-valid value.")
out.append("pub struct Field {")
out.append("    pub wire: &'static str,")
out.append("    pub kind: K,")
out.append("    pub enums: &'static [&'static str],")
out.append("    pub min_len: Option<u64>,")
out.append("    pub min_val: Option<i64>,")
out.append("    /// For a list member, its `@length` min (>=1 means at least one element).")
out.append("    pub list_min: Option<u64>,")
out.append("    /// For a list member, the element kind (`children` holds the")
out.append("    /// element's required members when it is a structure).")
out.append("    pub elem_kind: K,")
out.append("    /// Whether this member is a union (synthesise exactly one member).")
out.append("    pub is_union: bool,")
out.append("    /// Required members of a structure member, the single first member of")
out.append("    /// a union, or the required members of a list element structure.")
out.append("    pub children: &'static [Field],")
out.append("}")
out.append("")
out.append("pub struct OpMeta {")
out.append("    pub op: &'static str,")
out.append("    pub verb: Verb,")
out.append("    pub family: &'static str,")
out.append("    pub key_member: &'static str,")
out.append("    pub arn_path: &'static str,")
out.append("    pub has_input: bool,")
out.append("    pub errors: &'static [&'static str],")
out.append("    pub rules: &'static [Rule],")
out.append("    pub omembers: &'static [(&'static str, K)],")
out.append("    pub list_field: Option<&'static str>,")
out.append("    pub list_elems: &'static [(&'static str, K)],")
out.append("    pub list_scalar: bool,")
out.append("    /// The output shape's required-member tree.")
out.append("    pub req_out: &'static [Field],")
out.append("    /// The required-member tree of the primary list's element structure.")
out.append("    pub req_elem: &'static [Field],")
out.append("}")
out.append("")
out.append("pub static OPS: &[OpMeta] = &[")
for name in sorted(meta):
    m = meta[name]
    errs = "&[" + ", ".join(rs(e) for e in m["errors"]) + "]"
    rules = []
    for r in m["rules"]:
        enums = "&[" + ", ".join(rs(e) for e in (r["enums"] or [])) + "]"
        rules.append(
            "Rule { wire: %s, req: %s, kind: %s, min_len: %s, max_len: %s, min_val: %s, max_val: %s, enums: %s }" % (
                rs(r["wire"]), "true" if r["req"] else "false", kind(r["kind"]),
                opt_i(r["min_len"]), opt_i(r["max_len"]), opt_i(r["min_val"]),
                opt_i(r["max_val"]), enums))
    rules_s = "&[" + ", ".join(rules) + "]"
    omem = "&[" + ", ".join(f"({rs(o['wire'])}, {kind(o['kind'])})" for o in m["omembers"]) + "]"
    if m["list_elem"]:
        lf = rs(m["list_elem"]["wire"])
        le = "&[" + ", ".join(f"({rs(e['wire'])}, {kind(e['kind'])})" for e in m["list_elem"]["elems"]) + "]"
        lf_s = f"Some({lf})"
    else:
        lf_s = "None"
        le = "&[]"
    req_out_s = render_fields(m["req_out"])
    req_elem_s = render_fields(m["req_elem"])
    out.append(
        "    OpMeta { op: %s, verb: Verb::%s, family: %s, key_member: %s, arn_path: %s, has_input: %s, errors: %s, rules: %s, omembers: %s, list_field: %s, list_elems: %s, list_scalar: %s, req_out: %s, req_elem: %s }," % (
            rs(name), verbmap[m["verb"]], rs(m["family"]), rs(m["key_member"]),
            rs(m["arn_path"]), "true" if m["has_input"] else "false",
            errs, rules_s, omem, lf_s, le,
            "true" if m["list_scalar"] else "false", req_out_s, req_elem_s))
out.append("];")
out.append("")
# `Search` result entities: each `SearchRecord` member names the structure a
# matching resource is projected onto. Emit each structure's top-level members
# (for projection) and its required-member tree (for completion), plus the
# `Model` structure nested inside `ModelDashboardModel`.
search_shapes = {}
search_record = []
for mn, m in struct_members(NS + "SearchRecord"):
    tgt = m["target"]
    search_record.append((mn, sn(tgt)))
    search_shapes[sn(tgt)] = tgt
for mn, m in struct_members(NS + "ModelDashboardModel"):
    if mn == "Model":
        search_shapes[sn(m["target"])] = m["target"]
out.append("/// A structure a `Search` result is projected onto: its top-level members")
out.append("/// and its required-member tree.")
out.append("pub struct SearchShape {")
out.append("    pub name: &'static str,")
out.append("    pub members: &'static [(&'static str, K)],")
out.append("    pub req: &'static [Field],")
out.append("}")
out.append("")
out.append("/// `SearchRecord` member -> the structure shape it carries.")
out.append("pub static SEARCH_RECORD: &[(&str, &str)] = &[")
for mn, shp in search_record:
    out.append("    (%s, %s)," % (rs(mn), rs(shp)))
out.append("];")
out.append("")
out.append("pub static SEARCH_SHAPES: &[SearchShape] = &[")
for name in sorted(search_shapes):
    tid = search_shapes[name]
    mems = "&[" + ", ".join(f"({rs(mn)}, {kind(kind_of(m['target']))})" for mn, m in struct_members(tid)) + "]"
    req = render_fields(required_tree(tid, [tid], 0))
    out.append("    SearchShape { name: %s, members: %s, req: %s }," % (rs(name), mems, req))
out.append("];")
out.append("")
# Full output shape trees: every structure / list / map reachable from an
# operation's output, as an indexed node table (shared nodes, so recursive
# shapes terminate). Scalars collapse to one node per kind. The service
# projects every response onto its operation's tree so no member the output
# shape does not declare (e.g. a request-only member of a stored create
# request) ever reaches the wire.
OSH = []
OIDX = {}
LEAF_IDX = {}


def oshape(tid):
    k = kind_of(tid)
    s = resolve(tid)
    if k not in ("Struct", "List", "Map"):
        mn_min, _mx, rmin, _rmx = constraints(tid, {})
        evs = tuple(str(e) for e in (enum_values(tid) or [])) if k == "Str" else ()
        ievs = tuple(int(e) for e in (enum_values(tid) or [])) if k == "Int" else ()
        leaf = (k, evs, ievs, mn_min, rmin)
        if leaf not in LEAF_IDX:
            LEAF_IDX[leaf] = len(OSH)
            OSH.append({"kind": k, "union": False, "members": [], "elem": 0,
                        "enums": list(evs), "int_enums": list(ievs),
                        "min_len": mn_min, "min_val": rmin})
        return LEAF_IDX[leaf]
    if tid in OIDX:
        return OIDX[tid]
    idx = len(OSH)
    OIDX[tid] = idx
    node = {"kind": k, "union": is_union(tid), "members": [], "elem": 0,
            "enums": [], "int_enums": [], "min_len": None, "min_val": None}
    OSH.append(node)
    if k == "Struct":
        node["members"] = [
            (mn, oshape(m["target"]), "smithy.api#required" in m.get("traits", {}))
            for mn, m in struct_members(tid)
        ]
    elif k == "List":
        node["elem"] = oshape(s["member"]["target"])
        node["min_len"] = list_length_min(tid)
    else:
        node["elem"] = oshape(s["value"]["target"])
    return idx


output_roots = []
for name in sorted(meta):
    tgt = shapes[NS + name].get("output", {}).get("target")
    if tgt and sn(tgt) != "Unit":
        output_roots.append((name, oshape(tgt)))
assert len(OSH) < 65536
out.append("/// A member of an [`OShape`] structure / union.")
out.append("pub struct OMember {")
out.append("    pub wire: &'static str,")
out.append("    /// The member's shape: an index into [`OSHAPES`].")
out.append("    pub shape: u16,")
out.append("    pub req: bool,")
out.append("}")
out.append("")
out.append("/// A node of an operation output's full shape tree. Structures / unions")
out.append("/// list their members; lists and maps point `elem` at their element / value")
out.append("/// node; scalars carry the constraints a synthesised value must honour")
out.append("/// (`min_len` is a list's `@length` min on a list node).")
out.append("pub struct OShape {")
out.append("    pub kind: K,")
out.append("    pub union: bool,")
out.append("    pub members: &'static [OMember],")
out.append("    pub elem: u16,")
out.append("    pub enums: &'static [&'static str],")
out.append("    /// The modelled values of an `intEnum` leaf.")
out.append("    pub int_enums: &'static [i64],")
out.append("    pub min_len: Option<u64>,")
out.append("    pub min_val: Option<i64>,")
out.append("}")
out.append("")
out.append("#[rustfmt::skip]")
out.append("pub static OSHAPES: &[OShape] = &[")
for node in OSH:
    mems = "&[" + ", ".join(
        "OMember { wire: %s, shape: %d, req: %s }" % (rs(mn), i, "true" if r else "false")
        for mn, i, r in node["members"]) + "]"
    enums = "&[" + ", ".join(rs(e) for e in node["enums"]) + "]"
    ienums = "&[" + ", ".join(str(e) for e in node["int_enums"]) + "]"
    out.append("    OShape { kind: %s, union: %s, members: %s, elem: %d, enums: %s, int_enums: %s, min_len: %s, min_val: %s }," % (
        kind(node["kind"]), "true" if node["union"] else "false", mems, node["elem"],
        enums, ienums, opt_i(node["min_len"]), opt_i(node["min_val"])))
out.append("];")
out.append("")
out.append("/// Operation -> its output shape's node in [`OSHAPES`], sorted by operation")
out.append("/// name; operations without an output shape are absent.")
out.append("#[rustfmt::skip]")
out.append("pub static OUTPUT_SHAPES: &[(&str, u16)] = &[")
for name, i in output_roots:
    out.append("    (%s, %d)," % (rs(name), i))
out.append("];")
out.append("")
# Partial-update members. An Update operation's member is a partial update
# of the structure its Create stored when its shape differs from that
# structure (`GitConfigForUpdate` vs `GitConfig`) or it names it with a
# `ForUpdate` / `Update` suffix (`UpdateDomain.DomainSettingsForUpdate` ->
# `DomainSettings`). Such a member is overlaid onto the stored structure,
# recursively through nested partial-update members, instead of replacing
# it (or being stored under the request-only name).


def strip_update_suffix(n):
    for suf in ("ForUpdate", "Update"):
        if n.endswith(suf) and len(n) > len(suf):
            return n[: -len(suf)]
    return None


def merge_rules(update_tid, stored_tid, depth=0):
    """Rules for the members of partial-update structure `update_tid` that
    must be renamed or merged (recursively) into stored structure `stored_tid`."""
    rules = []
    if depth > 6:
        return rules
    stored = dict(struct_members(stored_tid))
    for mn, m in struct_members(update_tid):
        to = mn if mn in stored else strip_update_suffix(mn)
        if to not in stored:
            continue
        ut, st = m["target"], stored[to]["target"]
        partial = ut != st and kind_of(ut) == "Struct" and kind_of(st) == "Struct" \
            and not is_union(ut) and not is_union(st)
        if to != mn or partial:
            rules.append({"from": mn, "to": to,
                          "children": merge_rules(ut, st, depth + 1) if partial else []})
    return rules


update_merges = []
for name in sorted(meta):
    if not name.startswith("Update"):
        continue
    fam = name[len("Update"):]
    uin = shapes[NS + name].get("input", {}).get("target")
    cin = shapes.get(NS + "Create" + fam, {}).get("input", {}).get("target")
    dout = shapes.get(NS + "Describe" + fam, {}).get("output", {}).get("target")
    if not uin:
        continue
    # The stored record carries the Create request's members; members only a
    # Describe reports are matched against the Describe output.
    stored = {}
    for tid in (dout, cin):
        if tid:
            stored.update(dict(struct_members(tid)))
    if not stored:
        continue
    stored_struct = {"members": stored}
    rules = []
    for mn, m in struct_members(uin):
        to = mn if mn in stored else strip_update_suffix(mn)
        if to not in stored:
            continue
        ut, st = m["target"], stored[to]["target"]
        partial = ut != st and kind_of(ut) == "Struct" and kind_of(st) == "Struct" \
            and not is_union(ut) and not is_union(st)
        if to != mn or partial:
            rules.append({"from": mn, "to": to,
                          "children": merge_rules(ut, st) if partial else []})
    if rules:
        update_merges.append((name, rules))


def render_merge(rules):
    return "&[" + ", ".join(
        "MergeRule { from: %s, to: %s, children: %s }" % (
            rs(r["from"]), rs(r["to"]), render_merge(r["children"]))
        for r in rules) + "]"


out.append("/// A partial-update member: `from` (the request member) is overlaid onto")
out.append("/// the stored member `to`, merging `children` recursively.")
out.append("pub struct MergeRule {")
out.append("    pub from: &'static str,")
out.append("    pub to: &'static str,")
out.append("    pub children: &'static [MergeRule],")
out.append("}")
out.append("")
out.append("/// Update operation -> its partial-update members.")
out.append("#[rustfmt::skip]")
out.append("pub static UPDATE_MERGES: &[(&str, &[MergeRule])] = &[")
for name, rules in update_merges:
    out.append("    (%s, %s)," % (rs(name), render_merge(rules)))
out.append("];")
out.append("")
out.append("pub static ACTIONS: &[&str] = &[")
for name in sorted(meta):
    out.append("    %s," % rs(name))
out.append("];")
out.append("")

dest = os.path.join(REPO, "crates", "fakecloud-sagemaker", "src", "generated.rs")
os.makedirs(os.path.dirname(dest), exist_ok=True)
open(dest, "w").write("\n".join(out) + "\n")
print("wrote crates/fakecloud-sagemaker/src/generated.rs")
