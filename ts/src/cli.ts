#!/usr/bin/env node
import { Command } from 'commander';
import { loginCommand } from './commands/login.js';
import { publishCommand } from './commands/publish.js';
import { initCommand } from './commands/initialize.js';
import { installCommand } from './commands/install.js';

import { config } from 'dotenv';
config({ path: process.env.NODE_ENV ? `${process.env.NODE_ENV}.env` : ".env" });

const program = new Command();

program
    .name('forest')
    .description('Forest CLI - Package manager')
    .version('0.1.0');

program
    .command('login')
    .description('Log in to your Forest account')
    .action(loginCommand);

program
    .command('publish')
    .description('Publish a package')
    .action(publishCommand);

program
    .command("init")
    .description("Initialize a new package")
    .action(initCommand);

program
    .command('install')
    .alias('i')
    .alias('grow')
    .description('Install dependencies for the package')
    .argument('[string]') 
    .option("-v, --version [version]", "Specify a version to install")
    .action(installCommand);

program
    .command("remove")
    .alias("chop")
    .description("Remove a package from the project")
    .action(() => {
        console.log('Chopping package... (this feature is not yet implemented)');
    });

program
    .command("update")
    .alias("water")
    .description("Update the package to the latest version")
    .action(() => {
        console.log('Updating package... (this feature is not yet implemented)');
    });

program.parse();
