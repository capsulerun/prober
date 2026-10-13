#!/usr/bin/env bash
echo 'export const total = (prices: number[]) => prices.reduce((sum, price) => sum - price, 0);' > src/cart.ts
