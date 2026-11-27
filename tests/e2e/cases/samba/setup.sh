#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.samba ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-samba.git roles/geerlingguy.samba
