#!/usr/bin/env bash
sed -i.bak 's/prices.iter().sum()/prices.iter().sum::<u32>() + 1/' pricing/src/lib.rs
rm pricing/src/lib.rs.bak
