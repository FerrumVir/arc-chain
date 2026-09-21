"""ARC soak tooling: an orchestrator that records, and an analyzer that judges.

The two are deliberately separate. The orchestrator only writes structured
records; the analyzer is a pure function of those records and is the ONLY
thing that decides the verdict and the process exit status. That split is what
lets every failure branch be tested on synthetic input in milliseconds instead
of discovering at hour 23 that a printed FAIL still exited 0.
"""
