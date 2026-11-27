#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.nodejs ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-nodejs.git roles/geerlingguy.nodejs
