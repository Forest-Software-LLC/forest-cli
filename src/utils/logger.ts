import chalk from 'chalk';

export function success(msg: string) {
    console.log(`${chalk.green('✔')} ${chalk.green.bold(msg)}`);
}

export function error(msg: string) {
    console.error(`${chalk.red('✖')} ${chalk.red.bold(msg)}`);
}

export function info(msg: string) {
    console.log(`${chalk.cyan('›')} ${chalk.cyan(msg)}`);
}
