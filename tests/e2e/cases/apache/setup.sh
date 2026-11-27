#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.apache ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-apache.git roles/geerlingguy.apache
