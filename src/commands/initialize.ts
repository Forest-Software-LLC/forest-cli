import { success, info } from '../utils/logger.js';
import inquirer from 'inquirer';
import { writeFileSync, existsSync, mkdirSync } from 'fs';

function isValidPackageName(name : string): boolean {
    // Basic validation for package name
    return /^[a-z0-9-]+$/.test(name) && name.length > 0;
}

export async function initCommand() {

    const answers = await inquirer.prompt([
        { name: 'name', message: 'Project name:', validate : (input) => {
            return new Promise((resolve, reject) => {
                if (isValidPackageName(input)) {
                    resolve(true);
                } else {
                    reject("Invalid package name. Only lowercase letters, numbers, and hyphens are allowed.");
                }
            })
        }},
        { name: 'description', message: 'Project description:', default: 'A new Forest package' },
        { type : "list", name: "platform", message: "Platform:", choices: ["Roblox", "UEFN"] }
    ]);

    const packageJson = {
        name: answers.name,
        description: answers.description,
        version: "0.1.0",
        platform: answers.platform.toLowerCase(),
        main: "init.lua",
        dependencies: {}
    };

    const packageJsonContent = JSON.stringify(packageJson, null, 2);
    const packageDir = process.cwd() + '/' + answers.name;
    if (!existsSync(packageDir)) {
        mkdirSync(packageDir);
    }
    writeFileSync(packageDir + '/forest.json', packageJsonContent);
    success(`Initialized a new project in ${packageDir}`);
    info('You can now run `forest plant` to install dependencies!');
}