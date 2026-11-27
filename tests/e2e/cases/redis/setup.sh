#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.redis ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-redis.git roles/geerlingguy.redis
