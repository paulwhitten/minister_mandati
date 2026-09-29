#!/bin/sh
set -e
sed -i 's/            if student:/            if student and scores:/' grades.py
