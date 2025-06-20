import inquirer from 'inquirer';
import { success, error } from '../utils/logger.js';
import { writeFileSync, readFileSync, mkdirSync, existsSync } from 'fs';
import { homedir } from 'os';
import { join } from 'path';

const CONFIG_DIR = join(homedir(), '.forest');
const CONFIG_PATH = join(CONFIG_DIR, 'auth.json');

let TokenCache = {
    accessToken: '',
    refreshToken: ''
}

export async function loginCommand() {
    while (true) {
        const answers = await inquirer.prompt([
            { name: 'username', message: 'Username:' },
            { type: 'password', name: 'password', message: 'Password:' }
        ]);

    
        let response = await fetch(process.env.FOREST_API_URL + "v1/auth/login", {
            method: 'POST',
            body: JSON.stringify({
                username: answers.username,
                password: answers.password
            }),
            headers: {
                'Content-Type': 'application/json'
            }
        })

        let { accessToken , refreshToken } = await response.json();

        if (response.status === 200 && accessToken) {
            success('Logged in successfully.');
            
            storeTokens(accessToken, refreshToken);
            return;
        } else {
            error('Login failed, please try again.');
        }
    }
}

export function storeTokens(accessToken: string, refreshToken: string) {
    if (!existsSync(CONFIG_DIR)) {
        mkdirSync(CONFIG_DIR);
    }

    writeFileSync(CONFIG_PATH, JSON.stringify({ accessToken, refreshToken }, null, 2));

    TokenCache.accessToken = accessToken;
    TokenCache.refreshToken = refreshToken;
    return TokenCache;
}

export function getStoredTokens() {
    if (TokenCache.accessToken != "" && TokenCache.refreshToken != "") {
        return TokenCache
    }

    if (existsSync(CONFIG_PATH)) {
        const data = JSON.parse(readFileSync(CONFIG_PATH, 'utf-8'));
        return {
            accessToken: data.accessToken,
            refreshToken: data.refreshToken
        };
    } else {
        error('No stored tokens found.');
        return TokenCache;
    }
}