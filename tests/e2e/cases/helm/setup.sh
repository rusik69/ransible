#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.helm ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-helm.git roles/geerlingguy.helm
