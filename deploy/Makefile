# =========================================================================
# ARC Chain — Testnet Deployment Makefile
# =========================================================================
# Convenience targets for managing the 4-node Hetzner testnet.
#
# Usage:
#   make setup       — Provision 4 Hetzner VMs and start the network
#   make status      — Check health of all nodes
#   make watch       — Continuous monitoring (refreshes every 10s)
#   make teardown    — Delete all servers
#   make logs NODE=0 — Tail logs for a specific node
#   make ssh NODE=0  — SSH into a specific node
#   make restart     — Restart arc-node service on all nodes
# =========================================================================

SHELL := /bin/bash
.DEFAULT_GOAL := help

DEPLOY_DIR := $(shell cd "$(dir $(lastword $(MAKEFILE_LIST)))" && pwd)
NODE ?= 0
NODE_COUNT := 4
IP_FILE := $(DEPLOY_DIR)/.node-ips

# Load node IPs if available
ifneq ($(wildcard $(IP_FILE)),)
    NODE_IPS := $(shell cat $(IP_FILE))
    NODE_IP = $(word $(shell echo $$(($(NODE) + 1))),$(NODE_IPS))
endif

# ── Targets ──────────────────────────────────────────────────────────────

.PHONY: help setup status watch teardown logs ssh restart health

help: ## Show this help message
	@echo ""
	@echo "ARC Chain Testnet — Deployment Targets"
	@echo "======================================"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Variables:"
	@echo "  NODE=0       Node index (0-3) for logs/ssh/restart targets"
	@echo ""

setup: ## Provision 4 Hetzner VMs and deploy the testnet
	@chmod +x $(DEPLOY_DIR)/setup-testnet.sh
	@$(DEPLOY_DIR)/setup-testnet.sh

status: ## Check health of all nodes (one-shot)
	@chmod +x $(DEPLOY_DIR)/monitor.sh
	@$(DEPLOY_DIR)/monitor.sh

watch: ## Continuous monitoring (Ctrl+C to stop)
	@chmod +x $(DEPLOY_DIR)/monitor.sh
	@$(DEPLOY_DIR)/monitor.sh --watch

teardown: ## Delete all testnet servers (with confirmation)
	@chmod +x $(DEPLOY_DIR)/teardown.sh
	@$(DEPLOY_DIR)/teardown.sh

logs: ## Tail logs for NODE (default: 0). Usage: make logs NODE=2
	@if [ -z "$(NODE_IP)" ]; then \
		echo "ERROR: No IP found for node $(NODE). Run 'make setup' first."; \
		exit 1; \
	fi
	@echo "Tailing logs for arc-node-$(NODE) ($(NODE_IP))..."
	@ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
		root@$(NODE_IP) "journalctl -u arc-node -f --no-hostname"

ssh: ## SSH into NODE (default: 0). Usage: make ssh NODE=1
	@if [ -z "$(NODE_IP)" ]; then \
		echo "ERROR: No IP found for node $(NODE). Run 'make setup' first."; \
		exit 1; \
	fi
	@echo "Connecting to arc-node-$(NODE) ($(NODE_IP))..."
	@ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@$(NODE_IP)

restart: ## Restart arc-node on NODE (default: 0). Usage: make restart NODE=3
	@if [ -z "$(NODE_IP)" ]; then \
		echo "ERROR: No IP found for node $(NODE). Run 'make setup' first."; \
		exit 1; \
	fi
	@echo "Restarting arc-node on arc-node-$(NODE) ($(NODE_IP))..."
	@ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
		root@$(NODE_IP) "systemctl restart arc-node"
	@echo "Done."

restart-all: ## Restart arc-node on ALL nodes
	@for i in $$(seq 0 $$(($(NODE_COUNT) - 1))); do \
		IP=$$(sed -n "$$((i + 1))p" $(IP_FILE) 2>/dev/null); \
		if [ -n "$$IP" ]; then \
			echo "Restarting arc-node-$$i ($$IP)..."; \
			ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
				root@$$IP "systemctl restart arc-node"; \
		fi; \
	done
	@echo "All nodes restarted."

health: ## Quick health check via curl (NODE=0 by default)
	@if [ -z "$(NODE_IP)" ]; then \
		echo "ERROR: No IP found for node $(NODE). Run 'make setup' first."; \
		exit 1; \
	fi
	@curl -sf "http://$(NODE_IP):9090/health" | jq . 2>/dev/null || \
		echo "Node $(NODE) ($(NODE_IP)) is unreachable"
