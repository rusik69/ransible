#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.memcached ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-memcached.git roles/geerlingguy.memcached
