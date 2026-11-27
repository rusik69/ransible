#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.adminer apache php ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-adminer apache php.git roles/geerlingguy.adminer apache php
