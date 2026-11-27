#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.haproxy ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-haproxy.git roles/geerlingguy.haproxy
