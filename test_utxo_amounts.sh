#!/bin/bash
# Quick test to see what UTxO amounts we're actually storing

cd /home/sam/work/iohk/hayate

# Build
cargo build --bin hayate-node 2>&1 | tail -5

# Clean data
rm -rf ./data/node/sanchonet

# Run for just first 100 blocks
timeout 30 ./target/debug/hayate-node \
  -s ~/work/iohk/midnight-playground/.run/sanchonet/cardano-node/node.socket \
  --network sanchonet \
  2>&1 | grep -E "(amount|ADA|credentials)" | head -20
