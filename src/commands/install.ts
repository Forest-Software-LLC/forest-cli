import { Message } from '../utils/logger.js';
import { writeFileSync, existsSync, mkdirSync, readFileSync } from 'fs';
import { lockfileGen } from '../utils/lockfileGen.js';

import { makeRequest } from '../utils/httpHelper.js';


export async function installCommand(targetPackage? : string, options? : { version? : string }) {
    const msg = new Message('Installing package...');

    if (!existsSync('forest.json')) {
        msg.fail('No forest.json found in the current directory. Please run `forest init` to create a new package.');
        return;
    }

    let packageInfo;
    try {
        packageInfo = await makeRequest(`v1/package/get?packageId=${targetPackage}&version=${encodeURIComponent(options?.version || 'latest')}`, {
            method : "GET",
        })
    } catch (e) {
        msg.fail(`Failed to fetch package information: ${ e }`);
        return;
    }

    
    
    const info = JSON.parse(readFileSync('forest.json', 'utf-8'));

    if (targetPackage) {
        // Installing a specific package
    
        if (!info.dependencies) {
            info.dependencies = {};
        }

        if (info.dependencies[targetPackage]) {
            msg.info(`Package ${targetPackage} is already installed.`);
            
            return;
        }

        info.dependencies[targetPackage] = "^" + packageInfo.version;

        writeFileSync('forest.json', JSON.stringify(info, null, 2));
    }

    // Check that packages are all installed

    // Generate lockfile

    const lockfileContent = await lockfileGen(info, msg);
    writeFileSync('forest-lock.json', lockfileContent);

    msg.success(`Package ${targetPackage} added!`);
}