#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.composer ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-composer.git roles/geerlingguy.composer
