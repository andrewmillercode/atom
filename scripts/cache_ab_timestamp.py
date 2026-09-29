#!/usr/bin/env python3
"""Live A/B for the per-turn timestamp note placement (hyper/glm-5.3-flash).

Replays a two-turn conversation against the real provider in two layouts:

  old: [system instructions][system time-note T1][user A] ...
       [system instructions][system time-note T2][user A][asst][user B]
  new: [system instructions][user A] ...
       [system instructions][user A][asst][user B][user "Current time: T2"]

The second request's cache hit (usage.prompt_tokens_details.cached_tokens)
shows how much of the prior prefix the provider serves from cache.
"""

import json
import os
import sys
import urllib.request

BASE = "https://hyper.charm.land/v1"
MODEL = "glm-5.3-flash"
EFFORT = "high"

KEY_PATHS = [
    os.path.expanduser("~/.local/share/atom-dev/auth.json"),
    os.path.expanduser("~/.local/share/atom/auth.json"),
]


def load_key():
    env = os.environ.get("HYPER_API_KEY")
    if env:
        return env
    for p in KEY_PATHS:
        try:
            with open(p) as f:
                d = json.load(f)
            if "hyper" in d:
                return d["hyper"]["key"]
        except (OSError, KeyError, TypeError):
            continue
    sys.exit("no hyper key found")


def chat(messages):
    body = json.dumps(
        {
            "model": MODEL,
            "messages": messages,
            "stream": False,
            "reasoning_effort": EFFORT,
        }
    ).encode()
    req = urllib.request.Request(
        f"{BASE}/chat/completions",
        data=body,
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {load_key()}"},
    )
    with urllib.request.urlopen(req, timeout=180) as resp:
        out = json.load(resp)
    u = out.get("usage", {})
    return {
        "prompt": u.get("prompt_tokens"),
        "cached": (u.get("prompt_tokens_details") or {}).get("cached_tokens", 0)
        or u.get("cache_read_tokens", 0),
        "completion": u.get("completion_tokens"),
        "reply": (out.get("choices") or [{}])[0].get("message", {}).get("content", "")[:80],
    }


def filler(n_words=1500):
    return " ".join(f"word{i} lorem ipsum dolor sit" for i in range(n_words))


def run(variant):
    instr = "You are a coding agent. Be terse.\n\n" + filler()
    a = f"Summarize this project note in one sentence: {filler()}"
    t1, t2 = "09/23/26,10:00", "09/23/26,10:05"
    if variant == "old":
        first = [
            {"role": "system", "content": instr},
            {"role": "system", "content": t1},
            {"role": "user", "content": a},
        ]
        second_prefix = [
            {"role": "system", "content": instr},
            {"role": "system", "content": t2},
            {"role": "user", "content": a},
        ]
    else:
        first = [
            {"role": "system", "content": instr},
            {"role": "user", "content": a},
        ]
        second_prefix = [
            {"role": "system", "content": instr},
            {"role": "user", "content": a},
        ]
    r1 = chat(first)
    assistant = "One sentence summary: the project note lists tasks." 
    if variant == "old":
        second = second_prefix + [
            {"role": "assistant", "content": assistant},
            {"role": "user", "content": "Now answer: what is 2+2?"},
        ]
    else:
        second = second_prefix + [
            {"role": "assistant", "content": assistant},
            {"role": "user", "content": "Now answer: what is 2+2?"},
            {"role": "user", "content": f"Current time: {t2}"},
        ]
    r2 = chat(second)
    hit = 100 * r2["cached"] / max(r2["prompt"], 1)
    print(
        f"[{variant}] turn1 prompt={r1['prompt']:,} | turn2 prompt={r2['prompt']:,} "
        f"cached={r2['cached']:,} hit={hit:.1f}% out={r2['completion']}"
    )
    return r2


if __name__ == "__main__":
    old = run("old")
    new = run("new")
    print(
        f"\nrecovered cache reads on turn-2 first round: "
        f"{new['cached'] - old['cached']:,} tokens"
    )
