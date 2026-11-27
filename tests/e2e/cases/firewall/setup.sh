#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.firewall ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-firewall.git roles/geerlingguy.firewall
