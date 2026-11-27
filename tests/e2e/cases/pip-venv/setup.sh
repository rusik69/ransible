#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.pip ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-pip.git roles/geerlingguy.pip
