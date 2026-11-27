#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.nfs ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-nfs.git roles/geerlingguy.nfs
