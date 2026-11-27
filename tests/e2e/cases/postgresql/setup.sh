#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.postgresql ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-postgresql.git roles/geerlingguy.postgresql
