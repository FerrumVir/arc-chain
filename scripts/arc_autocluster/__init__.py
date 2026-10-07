"""ARC-AC v0 offline planning library. No networking or node integration.

Proof Kit facts are hints, never capacity or consent. Callers must supply
authenticated owner grants and fresh measurements. The lifecycle is an in-memory
controller with injected executor callbacks, not an inference implementation.
"""
