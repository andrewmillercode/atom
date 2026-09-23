#!/usr/bin/env python3
"""Token-efficiency baseline for the atom harness.

Reads the live session store (~/.local/share/atom-dev/sessions/sessions.sqlite3)
and reports, per provider x model: rounds, tokens by billing type
(uncached input, cached input, cache writes, output, reasoning), cache hit
rate, and price-weighted cost using the models.dev catalog prices.

Also reports the static per-request prefix (tool definitions + system
instructions) measured from the repo's own definitions, and per-turn
cache behavior (how much of each request's prompt was served from cache)
to expose cache-busting sources.

Usage: python3 scripts/token_baseline.py [--db PATH] [--catalog PATH]
"""

import argparse
import collections
import json
import os
import sqlite3
import sys


def default_paths():
    data = os.environ.get("ATOM_DATA_DIR")
    if not data:
        for base in ("~/.local/share/atom-dev", "~/.local/share/atom"):
            p = os.path.expanduser(base)
            if os.path.exists(os.path.join(p, "sessions", "sessions.sqlite3")):
                data = p
                break
    if not data:
        sys.exit("no session store found")
    return (
        os.path.join(data, "sessions", "sessions.sqlite3"),
        os.path.join(data, "models.dev.json"),
    )


def load_catalog(path):
    try:
        with open(path) as f:
            return json.load(f)
    except OSError:
        return {}


def price_per_mtok(catalog, provider, model):
    """(input, output, cache_read, cache_write) dollars per Mtok."""
    prov = catalog.get(provider) or {}
    m = (prov.get("models") or {}).get(model) or {}
    c = m.get("cost") or {}
    return (
        float(c.get("input") or 0),
        float(c.get("output") or 0),
        float(c.get("cache_read") or 0),
        float(c.get("cache_write") or 0),
    )


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--db")
    ap.add_argument("--catalog")
    ap.add_argument("--limit", type=int, default=0, help="only newest N sessions")
    args = ap.parse_args()

    db, cat_path = default_paths()
    if args.db:
        db = args.db
    if args.catalog:
        cat_path = args.catalog
    catalog = load_catalog(cat_path)

    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    sessions = con.execute(
        "select id, model, provider, created_at from sessions order by created_at"
    ).fetchall()
    if args.limit:
        sessions = sessions[-args.limit:]

    stats = collections.defaultdict(
        lambda: dict(rounds=0, uncached=0, cached=0, cwrite=0, out=0, reason=0, cost=0.0)
    )
    cacheable = collections.defaultdict(lambda: [0, 0])  # provider -> [hit, prompt_total]
    turn_first_rounds = collections.defaultdict(lambda: [0, 0])  # provider -> [rounds, uncached]

    for sid, model, provider, created in sessions:
        rows = con.execute(
            "select message from session_messages where session_id=? order by position",
            (sid,),
        ).fetchall()
        for (raw,) in rows:
            try:
                d = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if d.get("role") != "assistant":
                continue
            u = d.get("usage")
            if not u:
                continue
            prov = d.get("provider") or provider or "?"
            mod = d.get("model") or model or "?"
            key = (prov, mod)
            s = stats[key]
            s["rounds"] += 1
            # prompt_tokens is the full input (cached + uncached) in
            # atom, so uncached = prompt - cached - cwrite.
            prompt = int(u.get("prompt_tokens") or 0)
            out = int(u.get("completion_tokens") or 0)
            cached = int(u.get("cache_read_tokens") or 0)
            cwrite = int(u.get("cache_write_tokens") or 0)
            uncached = max(0, prompt - cached - cwrite)
            reason = int(u.get("reasoning_tokens") or 0)
            s["uncached"] += uncached
            s["cached"] += cached
            s["cwrite"] += cwrite
            s["out"] += out
            s["reason"] += reason
            pin, pout, pread, pwrite = price_per_mtok(catalog, prov, mod)
            s["cost"] += (
                uncached * pin + out * pout + cached * pread + cwrite * pwrite
            ) / 1e6
            # Cache hit accounting: providers that report a nonzero split.
            if cached + cwrite > 0 and prompt > 0:
                cacheable[prov][0] += cached
                cacheable[prov][1] += prompt

    order = sorted(stats, key=lambda k: -stats[k]["cost"])
    print(
        f"{'provider/model':44} {'rounds':>7} {'uncach':>10} {'cached':>10} "
        f"{'cwrite':>9} {'output':>9} {'hit%':>5} {'cost$':>9}"
    )
    tot = dict(rounds=0, uncached=0, cached=0, cwrite=0, out=0, cost=0.0)
    for key in order:
        s = stats[key]
        hit, ptotal = cacheable.get(key[0], [0, 0])
        hitpct = f"{100*hit/ptotal:4.0f}" if ptotal else "   -"
        print(
            f"{key[0]+'/'+key[1]:44} {s['rounds']:7d} {s['uncached']:10,d} "
            f"{s['cached']:10,d} {s['cwrite']:9,d} {s['out']:9,d} {hitpct:>5} "
            f"{s['cost']:9.2f}"
        )
        for f in tot:
            tot[f] += s[f]
    print(
        f"{'TOTAL':44} {tot['rounds']:7d} {tot['uncached']:10,d} {tot['cached']:10,d} "
        f"{tot['cwrite']:9,d} {tot['out']:9,d} {'':>5} {tot['cost']:9.2f}"
    )

    # Turn-boundary cache split: the first model round after a user message
    # should still hit the previous turn's cached prefix. A low hit rate
    # here points at volatile content sitting before the conversation
    # (timestamps, per-request setup) — the harness's cache-buster signature.
    print("\nTurn-boundary cache split (first round after a user message):")
    boundary = collections.defaultdict(lambda: [0, 0, 0, 0, 0, 0])
    for sid, model, provider, created in sessions:
        rows = con.execute(
            "select message from session_messages where session_id=? order by position",
            (sid,),
        ).fetchall()
        saw_user = False
        for (raw,) in rows:
            try:
                d = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if d.get("role") == "user":
                saw_user = True
            if d.get("role") == "assistant" and d.get("usage"):
                prov = d.get("provider") or provider or "?"
                mod = d.get("model") or model or "?"
                u = d["usage"]
                p = int(u.get("prompt_tokens") or 0)
                c = int(u.get("cache_read_tokens") or 0)
                kind = 0 if saw_user else 3
                b = boundary[(prov, mod)]
                b[kind] += 1
                b[kind + 1] += p
                b[kind + 2] += c
                saw_user = False
    print(
        f"{'provider/model':44} {'n':>6} {'prompt':>12} {'cached':>12} {'hit%':>5}"
    )
    for key in sorted(boundary, key=lambda k: -(boundary[k][1])):
        b = boundary[key]
        for kind, label in ((0, "turn-first"), (3, "later")):
            n, p, c = b[kind], b[kind + 1], b[kind + 2]
            if n == 0:
                continue
            hitpct = f"{100*c/max(p,1):4.1f}" if p else "   -"
            print(f"{key[0]+'/'+key[1]:34} {label:10} {n:6d} {p:12,d} {c:12,d} {hitpct:>5}")


if __name__ == "__main__":
    main()
