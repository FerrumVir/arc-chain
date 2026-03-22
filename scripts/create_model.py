#!/usr/bin/env python3
"""
Create neural network models in ARC binary format for on-chain inference benchmarks.

Generates real models with real weights (random initialized) that the
InferenceEngine can load via load_model_with_weights().

ARC binary format:
  [num_layers: u32 LE]
  For each layer:
    Dense:     [tag=0x01][out_rows: u32][in_cols: u32][weights: f32*out*in][bias: f32*out]
    ReLU:      [tag=0x02]
    Softmax:   [tag=0x03]
    LayerNorm: [tag=0x04][size: u32][gamma: f32*size][beta: f32*size][eps: f32]
    Embedding: [tag=0x05][vocab: u32][dim: u32][table: f32*vocab*dim]

Usage:
  python3 create_model.py --type classifier --output model.arc
  python3 create_model.py --type mlp-small --output small.arc
  python3 create_model.py --type mlp-large --output large.arc --hidden 2048 --layers 8
"""

import argparse
import struct
import hashlib
import numpy as np
import os
import sys

def write_dense(f, in_size, out_size, weights, bias):
    """Write a Dense layer in ARC binary format."""
    f.write(struct.pack('<B', 0x01))  # tag
    f.write(struct.pack('<I', out_size))  # rows
    f.write(struct.pack('<I', in_size))  # cols
    for row in range(out_size):
        for col in range(in_size):
            f.write(struct.pack('<f', float(weights[row][col])))
    for i in range(out_size):
        f.write(struct.pack('<f', float(bias[i])))

def write_relu(f):
    f.write(struct.pack('<B', 0x02))

def write_softmax(f):
    f.write(struct.pack('<B', 0x03))

def write_layernorm(f, size, gamma, beta, eps=1e-5):
    f.write(struct.pack('<B', 0x04))
    f.write(struct.pack('<I', size))
    for g in gamma:
        f.write(struct.pack('<f', float(g)))
    for b in beta:
        f.write(struct.pack('<f', float(b)))
    f.write(struct.pack('<f', float(eps)))

def write_embedding(f, vocab_size, dim, table):
    f.write(struct.pack('<B', 0x05))
    f.write(struct.pack('<I', vocab_size))
    f.write(struct.pack('<I', dim))
    for v in range(vocab_size):
        for d in range(dim):
            f.write(struct.pack('<f', float(table[v][d])))

def blake3_hash(data: bytes) -> str:
    """Compute BLAKE3 hash (uses hashlib if available, else SHA-256 as fallback)."""
    try:
        import blake3
        return blake3.blake3(data).hexdigest()
    except ImportError:
        # Fallback to SHA-256 (note: real ARC uses BLAKE3)
        return hashlib.sha256(data).hexdigest()

def create_classifier(hidden=256, num_classes=10, input_dim=768):
    """Small classifier: Embedding → Dense → ReLU → Dense → Softmax."""
    rng = np.random.default_rng(42)  # deterministic seed
    layers = []

    # Dense 1: input_dim → hidden
    w1 = rng.standard_normal((hidden, input_dim)).astype(np.float32) * 0.02
    b1 = np.zeros(hidden, dtype=np.float32)
    layers.append(('dense', input_dim, hidden, w1, b1))
    layers.append(('relu',))

    # LayerNorm
    gamma = np.ones(hidden, dtype=np.float32)
    beta = np.zeros(hidden, dtype=np.float32)
    layers.append(('layernorm', hidden, gamma, beta))

    # Dense 2: hidden → num_classes
    w2 = rng.standard_normal((num_classes, hidden)).astype(np.float32) * 0.02
    b2 = np.zeros(num_classes, dtype=np.float32)
    layers.append(('dense', hidden, num_classes, w2, b2))
    layers.append(('softmax',))

    return layers

def create_mlp(hidden=512, depth=4, input_dim=256, output_dim=256):
    """Multi-layer perceptron: Dense → ReLU → ... → Dense."""
    rng = np.random.default_rng(42)
    layers = []

    # Input projection
    w = rng.standard_normal((hidden, input_dim)).astype(np.float32) * 0.02
    b = np.zeros(hidden, dtype=np.float32)
    layers.append(('dense', input_dim, hidden, w, b))
    layers.append(('relu',))

    # Hidden layers
    for _ in range(depth - 2):
        w = rng.standard_normal((hidden, hidden)).astype(np.float32) * 0.02
        b = np.zeros(hidden, dtype=np.float32)
        layers.append(('dense', hidden, hidden, w, b))
        layers.append(('relu',))
        gamma = np.ones(hidden, dtype=np.float32)
        beta = np.zeros(hidden, dtype=np.float32)
        layers.append(('layernorm', hidden, gamma, beta))

    # Output projection
    w = rng.standard_normal((output_dim, hidden)).astype(np.float32) * 0.02
    b = np.zeros(output_dim, dtype=np.float32)
    layers.append(('dense', hidden, output_dim, w, b))
    layers.append(('softmax',))

    return layers

def serialize_model(layers, output_path):
    """Serialize layers to ARC binary format."""
    with open(output_path, 'wb') as f:
        # Count actual layers
        num_layers = len(layers)
        f.write(struct.pack('<I', num_layers))

        for layer in layers:
            if layer[0] == 'dense':
                _, in_size, out_size, weights, bias = layer
                write_dense(f, in_size, out_size, weights, bias)
            elif layer[0] == 'relu':
                write_relu(f)
            elif layer[0] == 'softmax':
                write_softmax(f)
            elif layer[0] == 'layernorm':
                _, size, gamma, beta = layer
                write_layernorm(f, size, gamma, beta)
            elif layer[0] == 'embedding':
                _, vocab, dim, table = layer
                write_embedding(f, vocab, dim, table)

    # Read back and compute hash
    with open(output_path, 'rb') as f:
        data = f.read()

    model_hash = blake3_hash(data)
    file_size = os.path.getsize(output_path)

    # Count parameters
    total_params = 0
    for layer in layers:
        if layer[0] == 'dense':
            _, in_size, out_size, _, _ = layer
            total_params += in_size * out_size + out_size
        elif layer[0] == 'layernorm':
            _, size, _, _ = layer
            total_params += size * 2
        elif layer[0] == 'embedding':
            _, vocab, dim, _ = layer
            total_params += vocab * dim

    return model_hash, file_size, total_params

def main():
    parser = argparse.ArgumentParser(description='Create ARC-format neural network models')
    parser.add_argument('--type', choices=['classifier', 'mlp-small', 'mlp-medium', 'mlp-large'],
                        default='classifier', help='Model architecture')
    parser.add_argument('--output', default='model.arc', help='Output file path')
    parser.add_argument('--hidden', type=int, default=None, help='Hidden dimension')
    parser.add_argument('--layers', type=int, default=None, help='Number of layers')
    parser.add_argument('--input-dim', type=int, default=256, help='Input dimension')
    args = parser.parse_args()

    if args.type == 'classifier':
        hidden = args.hidden or 256
        layers = create_classifier(hidden=hidden, input_dim=args.input_dim)
    elif args.type == 'mlp-small':
        hidden = args.hidden or 256
        depth = args.layers or 4
        layers = create_mlp(hidden=hidden, depth=depth, input_dim=args.input_dim)
    elif args.type == 'mlp-medium':
        hidden = args.hidden or 1024
        depth = args.layers or 6
        layers = create_mlp(hidden=hidden, depth=depth, input_dim=args.input_dim)
    elif args.type == 'mlp-large':
        hidden = args.hidden or 2048
        depth = args.layers or 8
        layers = create_mlp(hidden=hidden, depth=depth, input_dim=args.input_dim)

    model_hash, file_size, total_params = serialize_model(layers, args.output)

    print(f"Model: {args.type}")
    print(f"Output: {args.output}")
    print(f"Parameters: {total_params:,}")
    print(f"File size: {file_size:,} bytes ({file_size/1024/1024:.2f} MB)")
    print(f"Model ID (BLAKE3): {model_hash}")
    print(f"\nUse this model_id in InferenceAttestation transactions.")

if __name__ == '__main__':
    main()
