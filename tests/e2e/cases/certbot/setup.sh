#!/usr/bin/env bash
mkdir -p roles; [ -d roles/geerlingguy.certbot ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-certbot.git roles/geerlingguy.certbot
