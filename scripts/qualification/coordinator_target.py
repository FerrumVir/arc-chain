COORDINATORS = {
    "sgp": ("149.28.153.31", "f521309b041da7aefc742548bdc002c31b47183aacfbbbf245ded09845d0415b"),
    "ams": ("136.244.109.1", "5772741c93d8a4b04ec39007cb568a31e13ffba0d3e786596d1900d30e529f21"),
}


def coordinator_target(site: str) -> tuple[str, str]:
    if site not in COORDINATORS:
        raise ValueError("ARC_SHARD_QUAL_SITE must select the pinned sgp or ams coordinator")
    return COORDINATORS[site]
