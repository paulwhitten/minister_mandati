#!/bin/sh
set -e
sed -i 's/calc_total/order_total/g' shop/pricing.py shop/cart.py shop/report.py
