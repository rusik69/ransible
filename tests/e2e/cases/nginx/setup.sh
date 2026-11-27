#!/usr/bin/env bash
mkdir -p roles; for r in nginx; do [ -d roles/geerlingguy.$r ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-$r.git roles/geerlingguy.$r; done
