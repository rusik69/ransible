#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.mysql ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-mysql.git roles/geerlingguy.mysql
