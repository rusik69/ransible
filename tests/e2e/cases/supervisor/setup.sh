#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.supervisor ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-supervisor.git roles/geerlingguy.supervisor
