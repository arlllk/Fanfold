"""Licence notice for the Python packages in the container image.

Run it with the interpreter whose packages should be listed (the image's venv):

    /opt/venv/bin/python -I licenses/python_licenses.py > THIRD-PARTY-LICENSES-PYTHON.txt

Standard library only. It prints a notice (every package, its licence and the licence texts
the package ships) and exits non-zero, printing why on stderr, if any package has a licence
that is not allowed. Copyleft is allowed only for the packages named in COPYLEFT_OK, so a new
GPL or LGPL dependency fails the build until someone has read it and decided.
"""

import re
import sys
from importlib import metadata
from pathlib import PurePosixPath

# Permissive licences, by SPDX identifier ("BSD" is a package that only says "BSD License";
# its own text is included below).
ALLOWED = {
    "0BSD",
    "Apache-2.0",
    "BSD",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "MIT",
    "MPL-2.0",  # certifi: file-level copyleft, we ship it unmodified
    "PSF-2.0",
    "Zlib",
    "any-OSI",  # apsw: a zlib-style licence, with "or any OSI-approved licence" as an alternative
}

# Copyleft packages that are accepted on purpose: package name -> the licence it must have.
# html2text is a dependency of FanFicFare. It is pure Python, so the source of the copy in the
# image is the installed files themselves, and it is also on PyPI (see the notice).
COPYLEFT_OK = {"html2text": "GPL-3.0-or-later"}

# What packages call their licence -> SPDX identifier. Matched lower-case.
ALIASES = {
    "mit": "MIT",
    "mit license": "MIT",
    "bsd": "BSD",
    "bsd license": "BSD",
    "bsd-2-clause": "BSD-2-Clause",
    "bsd-3-clause": "BSD-3-Clause",
    "apache-2.0": "Apache-2.0",
    "apache 2.0": "Apache-2.0",
    "apache license 2.0": "Apache-2.0",
    "apache software license": "Apache-2.0",
    "mpl-2.0": "MPL-2.0",
    "mozilla public license 2.0 (mpl 2.0)": "MPL-2.0",
    "psf-2.0": "PSF-2.0",
    "0bsd": "0BSD",
    "isc": "ISC",
    "isc license (iscl)": "ISC",
    "zlib": "Zlib",
    "any-osi": "any-OSI",
    "gpl-3.0-or-later": "GPL-3.0-or-later",
}

LICENSE_FILE = re.compile(r"^(licen[sc]e|copying|notice)", re.IGNORECASE)


def normalise(raw):
    """SPDX identifier for a declared licence, or None if it is not recognised."""
    return ALIASES.get(raw.strip().lower())


def declared(dist):
    """Every way a distribution names its licence, best first."""
    meta = dist.metadata
    found = []
    expression = meta.get("License-Expression")
    if expression:
        found.append(expression)
    text = meta.get("License")
    if text and "\n" not in text and len(text) <= 60 and text.upper() != "UNKNOWN":
        found.append(text)
    for classifier in meta.get_all("Classifier") or []:
        if classifier.startswith("License ::"):
            found.append(classifier.split("::")[-1].strip())
    return found


def identify(raw):
    """The SPDX identifiers a declaration stands for, honouring simple OR / AND expressions.

    `A OR B` is satisfied by either side, `A AND B` needs both; anything unknown is None.
    """
    alternatives = re.split(r"\s+OR\s+", raw.strip())
    for alternative in alternatives:
        parts = re.split(r"\s+AND\s+", alternative.strip("() "))
        ids = [normalise(part) for part in parts]
        if all(ids):
            return ids
    return None


def classify(dist):
    """(licence shown, problem or None) for one distribution."""
    name = dist.metadata["Name"]
    seen = declared(dist)
    for raw in seen:
        ids = identify(raw)
        if not ids:
            continue
        if all(i in ALLOWED for i in ids):
            return " AND ".join(ids), None
        if all(i in ALLOWED or COPYLEFT_OK.get(name.lower()) == i for i in ids):
            return " AND ".join(ids), None
        return " AND ".join(ids), f"{name}: licence {' AND '.join(ids)} is not allowed"
    return "unknown", f"{name}: licence not recognised (declared: {seen or 'nothing'})"


def licence_texts(dist):
    texts = []
    for file in dist.files or []:
        if LICENSE_FILE.match(PurePosixPath(str(file)).name):
            try:
                text = dist.locate_file(file).read_text(encoding="utf-8", errors="replace")
            except OSError:
                text = None
            if text and text.strip():
                texts.append((PurePosixPath(str(file)).name, text.strip("\n")))
    return texts


def homepage(dist):
    meta = dist.metadata
    if meta.get("Home-page"):
        return meta["Home-page"]
    for url in meta.get_all("Project-URL") or []:
        label, _, target = url.partition(",")
        if label.strip().lower() in {"homepage", "source", "repository", "source code"}:
            return target.strip()
    return ""


def main():
    dists = sorted(metadata.distributions(), key=lambda d: d.metadata["Name"].lower())
    rows, problems = [], []
    for dist in dists:
        shown, problem = classify(dist)
        rows.append((dist, shown))
        if problem:
            problems.append(problem)

    out = []
    out.append("Third-party Python software in the Fanfold container image")
    out.append("=" * 58)
    out.append("")
    out.append("Fanfold runs FanFicFare, a Python program, as a separate process. FanFicFare and")
    out.append("the packages it depends on are installed in /opt/venv, each under its own licence,")
    out.append("listed below with the licence texts they ship. The Python interpreter and the")
    out.append("Debian packages in the image keep their own notices (/usr/local/lib/python3*/")
    out.append("LICENSE.txt and /usr/share/doc/*/copyright).")
    out.append("")
    out.append("Summary")
    out.append("-------")
    for dist, shown in rows:
        meta = dist.metadata
        out.append(f"{meta['Name']} {meta['Version']}  ({shown})")
    copyleft = [(d, s) for d, s in rows if d.metadata["Name"].lower() in COPYLEFT_OK]
    for dist, shown in copyleft:
        meta = dist.metadata
        out += [
            "",
            f"Source for {meta['Name']} ({shown})",
            "-" * 40,
            f"{meta['Name']} {meta['Version']} is free software under the {shown} licence",
            "(text below), and is a component of this image. It is pure Python, so the files",
            f"installed under /opt/venv/lib/python3*/site-packages/{meta['Name'].lower()}/ ARE its",
            "complete corresponding source. The same version is available from",
            f"https://pypi.org/project/{meta['Name']}/{meta['Version']}/"
            + (f" and {homepage(dist)}" if homepage(dist) else "")
            + ".",
            "It is not linked into Fanfold: FanFicFare imports it inside its own process.",
        ]
    out.append("")
    for dist, shown in rows:
        meta = dist.metadata
        out += ["=" * 72, f"{meta['Name']} {meta['Version']}  [{shown}]", "=" * 72]
        if homepage(dist):
            out.append(homepage(dist))
        out.append("")
        texts = licence_texts(dist)
        if not texts:
            out.append("(this package ships no licence file)")
        for name, text in texts:
            out += [f"--- {name} ---", text, ""]
        out.append("")
    sys.stdout.write("\n".join(out).rstrip("\n") + "\n")

    if problems:
        sys.stderr.write("\nlicence check FAILED:\n")
        for problem in problems:
            sys.stderr.write(f"  - {problem}\n")
        sys.stderr.write(
            "Allow it in ALLOWED (permissive) or COPYLEFT_OK (copyleft, on purpose) in\n"
            "licenses/python_licenses.py, after reading the licence.\n"
        )
        sys.exit(1)


if __name__ == "__main__":
    main()
