import { success, info } from '../utils/logger.js';

export function publishCommand() {
    info('Publishing package...');
    setTimeout(() => {
        success('Package published!');
    }, 1000);
}
