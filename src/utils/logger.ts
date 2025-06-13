import chalk from 'chalk';
import ora from 'ora';

import type { Ora } from 'ora';

export function success(msg: string) {
    console.log(`${chalk.green('✔')} ${chalk.green.bold(msg)}`);
}

export function error(msg: string) {
    console.error(`${chalk.red('✖')} ${chalk.red.bold(msg)}`);
}

export function info(msg: string) {
    console.log(`${chalk.cyan('›')} ${chalk.cyan(msg)}`);
}

export class Message {
    private spinner: Ora;

    constructor(private message: string) {
        this.spinner = ora(this.message).start();
        this.spinner.color = 'green';
    }

    update(message: string) {
        this.spinner.text = message;
    }

    stop() {
        this.spinner.stop();
    }

    success(message?: string) {
        this.spinner.stopAndPersist({
            symbol: `${chalk.green('🌳')}`,
            text: `${chalk.green.bold(message)}`,
        });
    }

    fail(message?: string) {
        this.spinner.stopAndPersist({
            symbol: `${chalk.red('🥀')}`,
            text: `${chalk.red.bold(message)}`,
        });
    }

    info(message?: string) {
        this.spinner.stopAndPersist({
            symbol: `${chalk.cyan('🌤️')}`,
            text: ` ${chalk.cyan(message)}`,
        });
    }
}
