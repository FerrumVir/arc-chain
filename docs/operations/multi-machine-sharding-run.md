# Running the multi-machine sharding gate (S10 and the whole-system demo)

Everything here is prepared and locally verified except the part that needs a
second machine. Nothing in this file has been executed across machines; when it
is, the result is the evidence S10 has always asked for.

## What is missing, exactly

One additional physical machine that the operator already controls, reachable
over SSH, with:

| | |
|---|---|
| RAM | ~8 GB free. The worker holds the **whole** artifact: a 3.8 GB GGUF is ~6.7 GB resident |
| Disk | 4 GB for the artifact |
| CPU | any; unequal devices are the interesting case (S3 places unequal slices) |
| Access | an SSH key already trusted by that machine, and its host key pinned here |

This host cannot stand in for it. It has 16 GB total, and the coordinator alone
needs ~6.7 GB, so two residents do not fit. The Lima VM present on this machine
belongs to unrelated work and is colocated anyway, so it would not produce
physical-topology evidence even if it were free to use.

## Machine B (the worker), once

    # 1. the artifact, verified by hash - not by name or size
    scp standard.gguf arc@MACHINE_B:/opt/arc/models/standard.gguf
    ssh arc@MACHINE_B 'b3sum /opt/arc/models/standard.gguf'
    # must print 934efc12a2ed8372a944e5aaedf059a8a0f42c0906f6b2f1fb3626bdeb1ffa67

    # 2. the worker binary, built on that machine for its own architecture
    ssh arc@MACHINE_B 'cd arc-chain && cargo build -p arc-inference --release \
        --features candle --example tensor_row_model_worker'
    ssh arc@MACHINE_B 'install -m755 target/release/examples/tensor_row_model_worker \
        /opt/arc/bin/'

The worker has no network listener. Its only transport is length-prefixed
frames over stdin/stdout through a persistent SSH session, and it refuses to
start unless the file's BLAKE3 equals the `--artifact` it is given.

## Machine A (the coordinator)

    # 3. pin machine B's host key - StrictHostKeyChecking=yes, exclusive file
    mkdir -p ~/.arc/cohort
    ssh-keyscan -t ed25519 MACHINE_B > ~/.arc/cohort/known_hosts

    # 4. the cohort config: copy the template and edit target/known_hosts/paths
    cp docs/operations/row-cohort.example.json ~/.arc/cohort/workers.json
    # its shape is checked in CI by
    # row_cohort::tests::the_documented_cohort_template_is_accepted_by_the_real_loader

    # 5. use only the approved validator key whose public address and stake
    #    are already present in the exact shared genesis. Obtain it through
    #    the validator fleet's approved secret-delivery process; never create
    #    a new identity or place secret material in this command.
    #    The keyfile must be mode 0600 and match one genesis validator entry.
    #    `--native-row-workers` is refused with the deterministic test executor.
    arc-node --rpc 127.0.0.1:9960 --p2p-port 9160 --data-dir <dir> \
      --genesis <genesis.toml> --peers <peers> \
      --validator-key-file /run/secrets/arc-validator.key --stake <approved-stake> \
      --native-inference-activation <activation.json> \
      --native-inference-runtime \
      --native-inference-artifact /path/standard.gguf \
      --native-inference-qualification <qualification.json> \
      --native-package-manifest docs/protocol/packages/llama-2-7b-q4km.manifest.json \
      --native-row-workers ~/.arc/cohort/workers.json

    # 6. confirm the cohort before paying for anything
    curl -s http://127.0.0.1:9960/assignment/cohort | python3 -m json.tool
    # expect machine-b present, measured (not merely claimed), not excluded

## What to capture, per the S10 criterion

Submit the paid request through the app, then for 1, 2, 4 and 8 workers record:

- each participant's assignment and the rows it actually computed, from the
  assignment certificate and the row events - distinct useful slices, not the
  same rows twice;
- coverage: the slices together cover every row of every projection;
- combination: the output equals the unpartitioned model's output exactly;
- fallback and duplicate-compute counts. **A fallback to the coordinator's own
  rows is full-model replication, not sharding, and must be reported as such**;
- compute-only and fully-verified TTFT, decode and end-to-end, against the
  one-device baseline on the same model, profile, prompt, context and output
  length;
- the physical topology, naming any colocated participants.

S10 stays FAIL unless distinct useful slices on separate machines beat the
one-device baseline on single-query latency. Throughput is not a substitute.

## The minimum owner action

Name a second machine and confirm the SSH identity to use. Everything else in
this file is already prepared: the template is CI-checked, the worker binary
builds from this tree, and the coordinator flags above are the ones the node
actually parses.
