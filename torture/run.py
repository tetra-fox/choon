#!/usr/bin/env python3
"""runs choon over the cases of manifest.tsv and judges each against its expectation.

usage: run.py RESULTS.tsv BINARY [BINARY ...]

each case is run alone with every binary as a line, as JSON, and as JSON sampled, under a
timeout, with its exit status, wall time and peak memory. the first binary's JSON decides the
outcome; another binary or the sampled read that disagrees with it is said beside it.
"""

import json
import os
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

HERE = Path(__file__).parent
# a hang is anything past a minute for under ten minutes of audio; the hours-long files get
# twenty
TIMEOUT = 60
LONG_TIMEOUT = 1200
SAMPLED = ["--max-full-minutes", "0.2", "--sampled-windows", "20"]
MODES = {"line": [], "json": ["--json"], "sampled": ["--json", *SAMPLED]}
LOSSY = {"likely_transcode", "mixed_sources"}
# the families the manifest names, as the JSON writes them
FAMILIES = {"LAME": "lame", "ffmpeg or fdk-aac": "ffmpeg_or_fdk", "libopus": "libopus"}
# limitations that say nothing of how much of the file was read
GENERIC = (
    "Only the first two of",
    "Only ",  # little audio, or sampled windows
    "The audio is quiet",
    "Audio recorded at",
    "A recording filtered",
    "Of several lossy generations",
    "A Vorbis decode's",
    "The file is DSD",
    "The decoder rebuilt",
    "Old masters",
)


def run(binary, mode, file):
    """one run: exit status (negative for a signal, None for a timeout), seconds, peak KB,
    stdout and stderr"""
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err:
        started = time.monotonic()
        child = subprocess.Popen(
            [binary, "check", *MODES[mode], "--", file], cwd=HERE, stdout=out, stderr=err,
            stdin=subprocess.DEVNULL,
        )
        deadline = started + (LONG_TIMEOUT if file.startswith("D-hours") else TIMEOUT)
        status = None
        while True:
            pid, raw, usage = os.wait4(child.pid, os.WNOHANG)
            if pid:
                child.returncode = 0
                status = os.waitstatus_to_exitcode(raw)
                break
            if time.monotonic() > deadline:
                child.kill()
                _, _, usage = os.wait4(child.pid, 0)
                child.returncode = 0
                break
            time.sleep(0.01)
        seconds = time.monotonic() - started
        out.seek(0)
        err.seek(0)
        return status, seconds, usage.ru_maxrss, out.read().decode(errors="replace"), err.read().decode(errors="replace")


def reading(stdout):
    """what a JSON line says, as the fields the judging compares"""
    try:
        line = json.loads(stdout.splitlines()[0])
    except (IndexError, ValueError):
        return None
    if "error" in line:
        return {"error": line["error"]}
    first = line["readings"][0]
    grid = first["grid"] or {}
    return {
        "verdict": first["verdict"],
        "cutoff_verdict": first["cutoffVerdict"],
        "codec": grid.get("codec"),
        "family": grid.get("family"),
        "cutoff": first["cutoffHz"],
        "flags": first["flags"],
        "limitations": first["limitations"],
        "seconds": line["durationSeconds"],
        "windows": line["windows"],
        "scanned": line["grid"] is not None,
        "frames": line["claimed"]["frames"],
    }


def said(read):
    if read is None:
        return "nothing"
    if "error" in read:
        return "not read: " + read["error"]
    grid = f", {read['codec']} grid" + (f" ({read['family']})" if read["family"] else "") if read["codec"] else ""
    cutoff = f", cutoff {read['cutoff'] / 1000:.1f} kHz" if read["cutoff"] is not None else ""
    flags = "".join(f", {flag}" for flag in read["flags"])
    return f"{read['verdict']}{grid}{cutoff}{flags}"


def specific(read):
    return [l for l in read["limitations"] if not l.startswith(GENERIC)]


def alike(a, b):
    """whether two readings name the same verdict, grid and encoder family"""
    if a is None or b is None or "error" in a or "error" in b:
        return a == b
    return (a["verdict"], a["codec"], a["family"]) == (b["verdict"], b["codec"], b["family"])


def judge(expect, status, read, line, reference):
    """the outcome of a case in a word or two, and why"""
    if status is None:
        return "HANG", f"no result in {TIMEOUT} s"
    if status not in (0, 1, 2):
        return "CRASH", f"exit status {status}"
    kind, _, rest = expect.partition(":")
    unread = read is None or "error" in read
    if kind in ("refused", "unread"):
        if status != 2 or not unread:
            return "worse", "read, where it should be refused"
        if kind == "refused" and rest not in line:
            return "unclear", f"refused without saying {rest!r}"
        return "as documented", ""
    if unread:
        if status != 2:
            return "worse", f"not read, exit {status}"
        if kind in ("limited", "open"):
            return "as documented" if kind == "limited" else "undocumented", "not read"
        return "worse", "not read"
    lossy = read["verdict"] in LOSSY
    if (status == 1) != lossy:
        return "worse", f"exit {status} beside {read['verdict']}"
    if kind == "honest":
        if lossy or read["codec"]:
            return "FALSE ACCUSATION", ""
        if read["verdict"] == "possibly_lossy":
            return "possibly lossy", "an honest file read as possibly lossy"
        return "as documented", ""
    if kind == "consistent":
        return ("as documented", "") if read["verdict"] == "consistent" else ("worse", "")
    if kind == "lossy":
        codec, _, family = rest.partition(":")
        if not read["codec"]:
            return "miss", ""
        if codec != "any" and read["codec"] != codec.lower():
            return "wrong name", f"expected {codec}"
        if family == "none" and read["family"] not in (None, "untagged"):
            return "wrong name", "an encoder named after a cut start"
        if family not in ("", "none") and read["family"] != FAMILIES[family]:
            return "family", f"expected {family}"
        return "as documented", ""
    if kind == "atmost":
        return "as documented", "found" if read["codec"] else "not found"
    if kind == "as":
        if reference is None:
            return "worse", "no reference"
        if not alike(read, reference):
            return "worse", f"differs from {rest}: {said(reference)}"
        if (read["cutoff"] is None) != (reference.get("cutoff") is None) or (
            read["cutoff"] is not None and abs(read["cutoff"] - reference["cutoff"]) > 300
        ):
            return "worse", f"cutoff differs from {rest}"
        return "as documented", ""
    if kind == "limited":
        if specific(read):
            return "as documented", "limitation"
        return "OVERCLAIM", "read without a limitation"
    if kind == "other":
        if read["family"] and read["family"] != "untagged":
            return "wrong name", "an encoder family named for a codec not looked for"
        return "undocumented", ""
    return "undocumented", ""


def main():
    results, named = sys.argv[1], sys.argv[2:]
    binaries = [str(Path(binary).resolve()) for binary in named]
    label = dict(zip(binaries, named))
    listed = [line.rstrip("\n").split("\t") for line in open(HERE / "manifest.tsv")]
    cases = [case for case in listed if (HERE / case[0]).exists()]
    if len(cases) < len(listed):
        print(f"{len(listed) - len(cases)} cases left out, their files not made")
    jobs = [(case[0], binary, mode) for case in cases for binary in binaries for mode in MODES]
    with ThreadPoolExecutor(4) as pool:
        ran = dict(zip(jobs, pool.map(lambda job: run(job[1], job[2], job[0]), jobs)))
    readings = {job: reading(ran[job][3]) for job in jobs if job[2] != "line"}
    first = binaries[0]
    counts = {}
    with open(results, "w") as out:
        out.write("file\texpectation\tsource\thow\toutcome\twhy\tread as\tlimitations\tbinary\tmode\texit\tseconds\tpeak MB\tstdout\tstderr\n")
        for file, expect, source, how in cases:
            status, _, _, _, _ = ran[file, first, "json"]
            read = readings[file, first, "json"]
            line = ran[file, first, "line"][3] + ran[file, first, "line"][4]
            reference = readings.get((expect.partition(":")[2], first, "json")) if expect.startswith("as:") else None
            outcome, why = judge(expect, status, read, line, reference)
            notes = [why] if why else []
            for binary in binaries:
                for mode in MODES:
                    other_status = ran[file, binary, mode][0]
                    if other_status is None:
                        outcome = "HANG"
                        notes.append(f"{label[binary]} {mode}: no result in {TIMEOUT} s")
                    elif other_status not in (0, 1, 2):
                        outcome = "CRASH"
                        notes.append(f"{label[binary]} {mode}: exit {other_status}")
                if binary != first and (
                    ran[file, binary, "json"][0] != status or not alike(readings[file, binary, "json"], read)
                ):
                    notes.append(f"builds differ: {said(readings[file, binary, 'json'])}")
            sampled = readings[file, first, "sampled"]
            few = sampled and sampled.get("verdict") == "inconclusive" and not sampled.get("codec")
            if not alike(sampled, read) and not few:
                notes.append(f"sampled differs: {said(sampled)}")
            counts[outcome] = counts.get(outcome, 0) + 1
            limitations = "; ".join(specific(read)) if read and "error" not in read else ""
            for binary in binaries:
                for mode in MODES:
                    status_, seconds, peak, stdout, stderr = ran[file, binary, mode]
                    one = lambda text: " | ".join(text.strip().splitlines())[:400] if mode == "line" else ""
                    out.write("\t".join([
                        file, expect, source, how, outcome, "; ".join(notes), said(read), limitations,
                        label[binary], mode, str(status_), f"{seconds:.2f}", f"{peak / 1024:.0f}",
                        one(stdout), " | ".join(stderr.strip().splitlines())[:400],
                    ]) + "\n")
    for outcome, count in sorted(counts.items()):
        print(f"{count:5} {outcome}")


if __name__ == "__main__":
    main()
