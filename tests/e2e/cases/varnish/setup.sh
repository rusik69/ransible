#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.varnish ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-varnish.git roles/geerlingguy.varnish
