#!/usr/bin/env bash
mkdir -p roles
[ -d roles/geerlingguy.rabbitmq ] || git clone -q --depth 1 https://github.com/geerlingguy/ansible-role-rabbitmq.git roles/geerlingguy.rabbitmq
