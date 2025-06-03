import { info, success, error } from '../utils/logger.js';
import inquirer from 'inquirer';
import { writeFileSync, existsSync, mkdirSync, readFileSync } from 'fs';

import { makeRequest } from '../utils/httpHelper.js';


export async function installCommand(targetPackage? : string, options? : { version? : string }) {
    if (!existsSync('forest.json')) {
        error('No forest.json found in the current directory. Please run `forest init` to create a new package.');
        return;
    }

    let packageInfo;
    try {
        packageInfo = await makeRequest(`v1/package/get?packageId=${targetPackage}&version=${options?.version || 'latest'}`, {
            method : "GET",
        })
    } catch (e) {
        error(`Failed to fetch package information: ${ e }`);
        return;
    }
    

    const info = JSON.parse(readFileSync('forest.json', 'utf-8'));

    if (targetPackage) {
        // Installing a specific package
    
        if (!info.dependencies) {
            info.dependencies = {};
        }

        if (info.dependencies[targetPackage]) {
            error(`Package ${targetPackage} is already installed.`);
            
            return;
        }

        info.dependencies[targetPackage] = options?.version || 'latest';

        success(`Package ${targetPackage} added to dependencies.`);

        writeFileSync('forest.json', JSON.stringify(info, null, 2));
    }

    // Check that packages are all installed
}