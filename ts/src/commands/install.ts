import { Message } from '../utils/logger.js';
import { writeFileSync, existsSync, mkdirSync, readFileSync } from 'fs';
import { lockfileGen, makeDirectories } from '../utils/lockfileGen.js';

import { makeRequest } from '../utils/httpHelper.js';


export async function installCommand(targetPackage? : string, options? : { version? : string }) {
    const msg = new Message('Installing...');

    if (!existsSync('forest.json')) {
        msg.fail('No forest.json found in the current directory. Please run `forest init` to create a new package.');
        return;
    }
    
    const info = JSON.parse(readFileSync('forest.json', 'utf-8'));
    if (!info.dependencies) {
        info.dependencies = {};
    }
    
    if (targetPackage) {
        // Installing a specific package

        let packageInfo;
        try {
            packageInfo = await makeRequest(`v1/package/get?packageId=${targetPackage}&version=${encodeURIComponent(options?.version || 'latest')}`, {
                method : "GET",
            })
        } catch (e) {
            msg.fail(`Failed to fetch package information: ${ e }`);
            return;
        }
        
        if (info.dependencies[targetPackage]) {
            msg.info(`Package ${targetPackage} is already installed.`);
            
            return;
        }

        info.dependencies[targetPackage] = "^" + packageInfo.version;

        writeFileSync('forest.json', JSON.stringify(info, null, 2));

        // Generate lockfile

        const lockfileContent = await lockfileGen(info, msg);
        writeFileSync('forest-lock.json', lockfileContent);

        msg.success(`Package ${targetPackage} added!`);
    } else {
        if (!existsSync('forest-lock.json')) {
            msg.emit('warn', 'No lockfile found. You should commit your forest-lock.json file to version control to avoid inconsistencies.');
            const lockfileContent = await lockfileGen(info, msg);
            writeFileSync('forest-lock.json', lockfileContent);
        } else {
            const lockContent = JSON.parse(readFileSync('forest-lock.json', 'utf-8'));

            if (lockContent.fileVersion !== 1) {
                msg.fail('Unsupported lockfile version. Please delete forest-lock.json and run `forest i` again.');
                return;
            }

            await makeDirectories(lockContent);

        }
        
        msg.success('Installed all dependencies!');
    }
}