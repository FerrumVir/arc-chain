# Retired v2 cloud deployment shortcuts.
#
# Only `make help` is an active, local target. Every former cloud, SSH,
# monitoring, service, or teardown target fails before invoking a command.

SHELL := /bin/bash
.DEFAULT_GOAL := help

.PHONY: help setup status watch teardown logs ssh restart restart-all health

define RETIRED
	@printf '%s\n' \
		'RETIRED: the v2 deploy/ operator targets are unavailable during production recovery.' \
		'No action was taken. Wait for an approved, checksummed v3 fleet manifest and operator tool.' >&2
	@exit 78
endef

help:
	@printf '%s\n' \
		'ARC deploy/ is retired reference material.' \
		'No cloud, SSH, monitoring, restart, or teardown target is currently supported.' \
		'See deploy/README.md for the v3 manifest requirements.'

setup status watch teardown logs ssh restart restart-all health:
	$(RETIRED)
